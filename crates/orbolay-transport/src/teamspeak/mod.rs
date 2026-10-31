use std::{
  collections::BTreeMap,
  time::{Duration, Instant},
};

use orbolay_core::{
  app_state::AppHandle,
  user::{User, UserVoiceState, VoiceSource},
};
use orbolay_logging::{success, warn};

mod client_query;

const RETRY_INTERVAL: Duration = Duration::from_secs(5);
const DISABLED_POLL_INTERVAL: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const READ_TIMEOUT: Duration = Duration::from_millis(200);

pub(crate) type TsResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Client {
  pub name: String,
  pub avatar: String,
  pub channel: u64,
  pub talking: bool,
  pub input_muted: bool,
  pub output_muted: bool,
  pub input_hardware: bool,
  pub output_hardware: bool,
}

impl Default for Client {
  fn default() -> Self {
    Self {
      name: "Unknown".to_string(),
      avatar: String::new(),
      channel: 0,
      talking: false,
      input_muted: false,
      output_muted: false,
      input_hardware: true,
      output_hardware: true,
    }
  }
}

impl Client {
  fn voice_state(&self) -> UserVoiceState {
    UserVoiceState::from_flags(
      self.output_muted || !self.output_hardware,
      self.input_muted || !self.input_hardware,
      self.talking,
    )
  }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Connection {
  pub own_client: u64,
  pub clients: BTreeMap<u64, Client>,
}

impl Connection {
  pub fn own_channel(&self) -> Option<u64> {
    self
      .clients
      .get(&self.own_client)
      .map(|client| client.channel)
      .filter(|channel| *channel != 0)
  }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Roster {
  source: VoiceSource,
  pub connections: BTreeMap<u64, Connection>,
}

impl Roster {
  pub fn new(source: VoiceSource) -> Self {
    Self {
      source,
      connections: BTreeMap::new(),
    }
  }

  pub fn users(&self) -> Vec<User> {
    let prefix = self.source.id_prefix();
    let mut users = Vec::new();

    for (connection_id, connection) in &self.connections {
      let Some(own_channel) = connection.own_channel() else {
        continue;
      };
      let members = connection
        .clients
        .iter()
        .filter(|(_, client)| client.channel == own_channel);

      for (client_id, client) in members {
        users.push(User {
          name: client.name.clone(),
          id: format!("{prefix}:{connection_id}:{client_id}"),
          avatar: client.avatar.clone(),
          voice_state: client.voice_state(),
          streaming: false,
          camera: false,
          volume: 100.,
          speaking_epoch: 0,
          source: self.source,
          is_local: *client_id == connection.own_client,
        });
      }
    }

    users
  }

  pub fn client_mut(&mut self, connection_id: u64, client_id: u64) -> Option<&mut Client> {
    self
      .connections
      .get_mut(&connection_id)?
      .clients
      .get_mut(&client_id)
  }

  pub fn publish(&self, app: &AppHandle) {
    let users = self.users();
    app.update_if_changed(|state| state.set_teamspeak_users(self.source, users));
  }
}

pub(crate) fn is_timeout(error: &std::io::Error) -> bool {
  matches!(
    error.kind(),
    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
  )
}

pub(crate) fn is_enabled(app: &AppHandle, source: VoiceSource) -> bool {
  app.read(|state| match source {
    VoiceSource::TeamSpeak3 => state.config.teamspeak3_enabled,
    VoiceSource::TeamSpeak6 => state.config.teamspeak6_enabled,
    VoiceSource::Discord => false,
  })
}

fn wait_while_enabled(app: &AppHandle, source: VoiceSource, delay: Duration) {
  let deadline = Instant::now() + delay;

  while Instant::now() < deadline && is_enabled(app, source) {
    std::thread::sleep(DISABLED_POLL_INTERVAL);
  }
}

fn spawn_backend(app: AppHandle, source: VoiceSource, session: fn(&AppHandle) -> TsResult) {
  std::thread::spawn(move || {
    let name = source.display_name();
    let mut last_error = String::new();

    loop {
      if !is_enabled(&app, source) {
        last_error.clear();
        std::thread::sleep(DISABLED_POLL_INTERVAL);
        continue;
      }

      let retry = match session(&app) {
        Ok(()) => {
          success!("{} session closed", name);
          last_error.clear();
          RETRY_INTERVAL
        }
        Err(e) => {
          let error = e.to_string();
          if error != last_error {
            warn!("{} session failed: {}", name, error);
            last_error = error;
          }

          RETRY_INTERVAL
        }
      };

      app.update_if_changed(|state| state.set_teamspeak_users(source, vec![]));
      wait_while_enabled(&app, source, retry);
    }
  });
}

pub fn start_teamspeak_threads(app: AppHandle) {
  spawn_backend(app, VoiceSource::TeamSpeak3, client_query::run_session);
}

#[cfg(test)]
pub(crate) mod tests {
  use std::sync::{Arc, RwLock};

  use orbolay_core::app_state::AppState;

  use super::*;

  pub fn wait_for(app: &AppHandle, check: impl Fn(&AppState) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !app.read(&check) {
      assert!(Instant::now() < deadline, "timed out waiting for state");
      std::thread::sleep(Duration::from_millis(10));
    }
  }

  pub fn test_app(configure: impl FnOnce(&mut AppState)) -> AppHandle {
    let mut state = AppState::new();
    configure(&mut state);
    let (redraw_tx, _) = flume::unbounded();
    AppHandle::new(Arc::new(RwLock::new(state)), redraw_tx)
  }

  pub fn roster_with(source: VoiceSource, own_client: u64, clients: &[(u64, u64)]) -> Roster {
    let mut roster = Roster::new(source);
    let connection = roster.connections.entry(1).or_default();
    connection.own_client = own_client;

    for (id, channel) in clients {
      let client = Client {
        channel: *channel,
        ..Default::default()
      };
      connection.clients.insert(*id, client);
    }

    roster
  }

  #[test]
  fn only_lists_clients_in_own_channel() {
    let roster = roster_with(VoiceSource::TeamSpeak3, 5, &[(5, 10), (6, 10), (7, 11)]);

    let users = roster.users();

    assert_eq!(
      users.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
      ["ts3:1:5", "ts3:1:6"]
    );
    assert!(users[0].is_local);
    assert!(!users[1].is_local);
    assert_eq!(users[0].source, VoiceSource::TeamSpeak3);
  }

  #[test]
  fn lists_nobody_until_own_client_is_in_a_channel() {
    let roster = roster_with(VoiceSource::TeamSpeak6, 5, &[(6, 10)]);

    assert!(roster.users().is_empty());
  }

  #[test]
  fn mute_takes_priority_over_talking() {
    let talking = Client {
      talking: true,
      ..Default::default()
    };
    assert_eq!(talking.voice_state(), UserVoiceState::Speaking);

    let muted = Client {
      input_muted: true,
      ..talking.clone()
    };
    assert_eq!(muted.voice_state(), UserVoiceState::Muted);

    let no_mic = Client {
      input_hardware: false,
      ..talking.clone()
    };
    assert_eq!(no_mic.voice_state(), UserVoiceState::Muted);

    let deafened = Client {
      input_muted: true,
      output_muted: true,
      ..talking
    };
    assert_eq!(deafened.voice_state(), UserVoiceState::Deafened);
  }

  #[test]
  fn teamspeak_sources_do_not_overwrite_each_other_or_discord() {
    let mut state = AppState::new();
    let ts3 = roster_with(VoiceSource::TeamSpeak3, 1, &[(1, 1)]);
    let ts6 = roster_with(VoiceSource::TeamSpeak6, 1, &[(1, 1)]);

    assert!(state.set_teamspeak_users(VoiceSource::TeamSpeak3, ts3.users()));
    assert!(!state.set_teamspeak_users(VoiceSource::TeamSpeak3, ts3.users()));
    assert!(state.set_teamspeak_users(VoiceSource::TeamSpeak6, ts6.users()));
    assert_eq!(state.teamspeak_users.len(), 2);

    let mut discord = ts3.users().remove(0);
    discord.source = VoiceSource::Discord;
    discord.is_local = false;
    discord.id = "1234".to_string();
    state.user_id = discord.id.clone();
    state.voice_users.push(discord);

    let merged = state.all_voice_users();
    assert_eq!(merged.len(), 3);
    assert!(merged[0].is_local);

    state.voice_users.clear();
    assert_eq!(state.all_voice_users().len(), 2);

    assert!(state.set_teamspeak_users(VoiceSource::TeamSpeak3, vec![]));
    assert_eq!(state.teamspeak_users.len(), 1);
    assert_eq!(state.teamspeak_users[0].source, VoiceSource::TeamSpeak6);
  }
}
