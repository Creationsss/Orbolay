use std::net::{SocketAddr, TcpStream};

use orbolay_core::{app_state::AppHandle, config::save_config, user::VoiceSource};
use orbolay_logging::success;
use serde_json::{Value, json};
use tungstenite::{Message, Utf8Bytes};

use super::{
  CONNECT_TIMEOUT, Client, READ_TIMEOUT, Rejected, Roster, TsResult, is_enabled, is_timeout,
};

const SOURCE: VoiceSource = VoiceSource::TeamSpeak6;
const DEFAULT_PORT: u16 = 5899;
const IDENTIFIER: &str = "dev.spikehd.orbolay";
const AVATAR_BUCKET: &str = "https://storage.googleapis.com/ts-sys-myts-avatars/";

fn id_at(value: &Value, key: &str) -> Option<u64> {
  let value = value.get(key)?;

  value
    .as_u64()
    .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

fn as_flag(value: &Value) -> Option<bool> {
  value
    .as_bool()
    .or_else(|| value.as_u64().map(|n| n != 0))
    .or_else(|| value.as_str().map(|s| s == "1" || s == "true"))
}

fn avatar_url(value: &str) -> Option<String> {
  let (_, url) = value.split_once(',')?;
  let path = url.strip_prefix(AVATAR_BUCKET)?;
  let is_safe = !path.is_empty()
    && path
      .chars()
      .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_'));

  is_safe.then(|| url.to_string())
}

fn apply_property(client: &mut Client, key: &str, value: &Value) {
  let flag = match key {
    "nickname" => {
      if let Some(name) = value.as_str() {
        client.name = name.to_string();
      }
      return;
    }
    "myteamspeakAvatar" => {
      client.avatar = value.as_str().and_then(avatar_url).unwrap_or_default();
      return;
    }
    "flagTalking" => &mut client.talking,
    "inputMuted" => &mut client.input_muted,
    "outputMuted" => &mut client.output_muted,
    "inputHardware" => &mut client.input_hardware,
    "outputHardware" => &mut client.output_hardware,
    _ => return,
  };

  if let Some(value) = as_flag(value) {
    *flag = value;
  }
}

fn apply_properties(client: &mut Client, properties: &Value) {
  for (key, value) in properties.as_object().into_iter().flatten() {
    apply_property(client, key, value);
  }
}

fn load_auth(roster: &mut Roster, payload: &Value) {
  roster.connections.clear();

  let connections = payload.get("connections").and_then(Value::as_array);

  for connection in connections.into_iter().flatten() {
    let Some(id) = id_at(connection, "id") else {
      continue;
    };

    let entry = roster.connections.entry(id).or_default();
    entry.own_client = id_at(connection, "clientId").unwrap_or(0);

    let clients = connection.get("clientInfos").and_then(Value::as_array);

    for info in clients.into_iter().flatten() {
      let Some(client_id) = id_at(info, "id") else {
        continue;
      };

      let client = entry.clients.entry(client_id).or_default();
      client.channel = id_at(info, "channelId").unwrap_or(0);
      apply_properties(client, &info["properties"]);
    }
  }
}

fn apply_event(roster: &mut Roster, event: &str, payload: &Value) -> Option<()> {
  let connection_id = id_at(payload, "connectionId")?;

  match event {
    "connectStatusChanged" => {
      if id_at(payload, "status")? == 0 {
        roster.connections.remove(&connection_id);
      } else if let Some(client_id) = id_at(&payload["info"], "clientId") {
        roster
          .connections
          .entry(connection_id)
          .or_default()
          .own_client = client_id;
      }
    }
    "clientMoved" => {
      let client_id = id_at(payload, "clientId")?;
      let new_channel = id_at(payload, "newChannelId")?;
      let connection = roster.connections.entry(connection_id).or_default();

      if new_channel == 0 {
        connection.clients.remove(&client_id);
      } else {
        let client = connection.clients.entry(client_id).or_default();
        client.channel = new_channel;
        apply_properties(client, &payload["properties"]);
      }
    }
    "clientPropertiesUpdated" => {
      let client = roster.client_mut(connection_id, id_at(payload, "clientId")?)?;
      apply_properties(client, &payload["properties"]);
    }
    "clientSelfPropertyUpdated" => {
      let own_client = roster.connections.get(&connection_id)?.own_client;
      let client = roster.client_mut(connection_id, own_client)?;
      apply_property(client, payload["flag"].as_str()?, &payload["newValue"]);
    }
    "talkStatusChanged" => {
      let client = roster.client_mut(connection_id, id_at(payload, "clientId")?)?;
      client.talking = id_at(payload, "status")? == 1;
    }
    _ => {}
  }

  Some(())
}

fn auth_request(api_key: &str) -> Value {
  json!({
    "type": "auth",
    "payload": {
      "identifier": IDENTIFIER,
      "version": env!("CARGO_PKG_VERSION"),
      "name": "Orbolay",
      "description": "Shows who is talking in your channel on the Orbolay overlay",
      "content": {
        "apiKey": api_key,
      },
    },
  })
}

fn store_api_key(app: &AppHandle, api_key: Option<String>) {
  let updated = app.update(|state| {
    if state.config.teamspeak6_api_key == api_key {
      return None;
    }

    state.config.teamspeak6_api_key = api_key;
    Some(state.config.clone())
  });

  if let Some(config) = updated {
    save_config(&config);
  }
}

fn handle_auth(app: &AppHandle, roster: &mut Roster, message: &Value, api_key: &str) -> TsResult {
  let status = &message["status"];

  if status["code"].as_i64().is_some_and(|code| code != 0) {
    let reason = status["message"]
      .as_str()
      .unwrap_or("unknown error")
      .to_string();

    if api_key.is_empty() {
      return Err(Rejected(reason).into());
    }

    store_api_key(app, None);
    return Err(format!("stored API key rejected: {reason}").into());
  }

  let payload = &message["payload"];

  if let Some(key) = payload["apiKey"].as_str() {
    store_api_key(app, Some(key.to_string()));
  }

  load_auth(roster, payload);
  success!("Connected to {}", SOURCE.display_name());
  Ok(())
}

pub(super) fn run_session(app: &AppHandle) -> TsResult {
  let port = app
    .read(|state| state.config.teamspeak6_port)
    .unwrap_or(DEFAULT_PORT);
  run_session_at(app, SocketAddr::from(([127, 0, 0, 1], port)))
}

fn run_session_at(app: &AppHandle, addr: SocketAddr) -> TsResult {
  let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
  stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;

  let (mut socket, _) =
    tungstenite::client(format!("ws://{addr}"), stream).map_err(|e| e.to_string())?;
  socket.get_mut().set_read_timeout(Some(READ_TIMEOUT))?;

  let api_key = app
    .read(|state| state.config.teamspeak6_api_key.clone())
    .unwrap_or_default();
  socket.send(Message::Text(Utf8Bytes::from(
    auth_request(&api_key).to_string(),
  )))?;

  let mut roster = Roster::new(SOURCE);

  while is_enabled(app, SOURCE) {
    let message = match socket.read() {
      Ok(Message::Text(text)) => text,
      Ok(Message::Close(_)) => return Ok(()),
      Ok(_) => continue,
      Err(tungstenite::Error::Io(ref e)) if is_timeout(e) => continue,
      Err(e) => return Err(e.into()),
    };

    let message: Value = serde_json::from_str(message.as_str())?;

    match message["type"].as_str() {
      Some("auth") => handle_auth(app, &mut roster, &message, &api_key)?,
      Some(event) => {
        apply_event(&mut roster, event, &message["payload"]);
      }
      None => {}
    }

    roster.publish(app);
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use std::{net::TcpListener, sync::mpsc};

  use orbolay_core::user::{User, UserVoiceState};

  use super::*;
  use crate::teamspeak::tests::{test_app, wait_for};

  fn auth_payload() -> Value {
    json!({
      "apiKey": "issued-key",
      "connections": [{
        "id": 1,
        "clientId": 5,
        "clientInfos": [
          { "id": 5, "channelId": 7, "properties": { "nickname": "Me", "inputMuted": false } },
          { "id": 6, "channelId": "7", "properties": { "nickname": "Friend", "outputMuted": true } },
          { "id": 9, "channelId": 2, "properties": { "nickname": "Elsewhere" } }
        ]
      }, {
        "id": 2,
        "clientId": 0
      }]
    })
  }

  fn names(roster: &Roster) -> Vec<String> {
    roster.users().into_iter().map(|user| user.name).collect()
  }

  fn user_named(roster: &Roster, name: &str) -> User {
    roster
      .users()
      .into_iter()
      .find(|user| user.name == name)
      .unwrap()
  }

  fn state_of(roster: &Roster, name: &str) -> UserVoiceState {
    user_named(roster, name).voice_state
  }

  fn loaded_roster() -> Roster {
    let mut roster = Roster::new(VoiceSource::TeamSpeak6);
    load_auth(&mut roster, &auth_payload());
    roster
  }

  #[test]
  fn auth_payload_seeds_own_channel_and_accepts_string_ids() {
    let roster = loaded_roster();

    assert_eq!(names(&roster), ["Me", "Friend"]);
    assert_eq!(state_of(&roster, "Friend"), UserVoiceState::Deafened);
  }

  #[test]
  fn accepts_only_myteamspeak_avatar_urls() {
    let good = format!("2,{AVATAR_BUCKET}0123-abc/1539736415");

    assert_eq!(avatar_url(&good).as_deref(), Some(&good[2..]));
    assert_eq!(avatar_url(""), None);
    assert_eq!(avatar_url("2,https://evil.example/track.png"), None);
    assert_eq!(avatar_url(&format!("2,{AVATAR_BUCKET}")), None);
    assert_eq!(avatar_url(&format!("2,{AVATAR_BUCKET}../other")), None);
    assert_eq!(avatar_url(&format!("2,{AVATAR_BUCKET}a\"><b>")), None);
    assert_eq!(avatar_url(&format!("2,{AVATAR_BUCKET}a?x=1")), None);
    assert_eq!(
      avatar_url("2,http://storage.googleapis.com/ts-sys-myts-avatars/a"),
      None
    );
  }

  #[test]
  fn avatars_come_from_properties_and_clear_when_invalid() {
    let mut roster = loaded_roster();
    let url = format!("{AVATAR_BUCKET}id/1");

    apply_event(
      &mut roster,
      "clientPropertiesUpdated",
      &json!({
        "connectionId": 1, "clientId": 6,
        "properties": { "myteamspeakAvatar": format!("2,{url}") }
      }),
    );
    assert_eq!(user_named(&roster, "Friend").avatar, url);

    apply_event(
      &mut roster,
      "clientPropertiesUpdated",
      &json!({
        "connectionId": 1, "clientId": 6,
        "properties": { "myteamspeakAvatar": "2,https://evil.example/x" }
      }),
    );
    assert_eq!(user_named(&roster, "Friend").avatar, "");
  }

  #[test]
  fn denied_first_authorization_is_a_rejection() {
    let app = test_app(|_| {});
    let mut roster = Roster::new(VoiceSource::TeamSpeak6);
    let message = json!({
      "type": "auth",
      "payload": {},
      "status": { "code": 1, "message": "denied" }
    });

    let error = handle_auth(&app, &mut roster, &message, "").unwrap_err();

    assert!(error.is::<Rejected>());
    assert!(error.to_string().contains("denied"));
  }

  #[test]
  fn talk_status_sets_and_clears_speaking() {
    let mut roster = loaded_roster();
    let talk = |status: u64, roster: &mut Roster| {
      apply_event(
        roster,
        "talkStatusChanged",
        &json!({ "connectionId": 1, "clientId": 5, "isWhisper": false, "status": status }),
      )
    };

    talk(1, &mut roster);
    assert_eq!(state_of(&roster, "Me"), UserVoiceState::Speaking);

    talk(0, &mut roster);
    assert_eq!(state_of(&roster, "Me"), UserVoiceState::NotSpeaking);

    talk(1, &mut roster);
    talk(2, &mut roster);
    assert_eq!(state_of(&roster, "Me"), UserVoiceState::NotSpeaking);
  }

  #[test]
  fn client_moves_update_membership() {
    let mut roster = loaded_roster();

    apply_event(
      &mut roster,
      "clientMoved",
      &json!({
        "connectionId": 1, "clientId": 10, "oldChannelId": 0, "newChannelId": 7,
        "properties": { "nickname": "Joiner" }
      }),
    );
    assert!(names(&roster).contains(&"Joiner".to_string()));

    apply_event(
      &mut roster,
      "clientMoved",
      &json!({ "connectionId": 1, "clientId": 10, "oldChannelId": 7, "newChannelId": 0 }),
    );
    apply_event(
      &mut roster,
      "clientMoved",
      &json!({ "connectionId": 1, "clientId": 6, "oldChannelId": 7, "newChannelId": 2 }),
    );
    assert_eq!(names(&roster), ["Me"]);
  }

  #[test]
  fn moving_own_client_switches_visible_channel() {
    let mut roster = loaded_roster();

    apply_event(
      &mut roster,
      "clientMoved",
      &json!({ "connectionId": 1, "clientId": 5, "oldChannelId": 7, "newChannelId": 2 }),
    );

    assert_eq!(names(&roster), ["Me", "Elsewhere"]);
  }

  #[test]
  fn partial_property_updates_keep_other_fields() {
    let mut roster = loaded_roster();

    apply_event(
      &mut roster,
      "clientPropertiesUpdated",
      &json!({ "connectionId": 1, "clientId": 6, "properties": { "outputMuted": false } }),
    );
    assert_eq!(state_of(&roster, "Friend"), UserVoiceState::NotSpeaking);

    apply_event(
      &mut roster,
      "clientPropertiesUpdated",
      &json!({ "connectionId": 1, "clientId": 6, "properties": { "inputHardware": false } }),
    );
    assert_eq!(state_of(&roster, "Friend"), UserVoiceState::Muted);
  }

  #[test]
  fn self_property_updates_apply_to_own_client() {
    let mut roster = loaded_roster();

    apply_event(
      &mut roster,
      "clientSelfPropertyUpdated",
      &json!({ "connectionId": 1, "flag": "inputMuted", "newValue": true, "oldValue": false }),
    );
    assert_eq!(state_of(&roster, "Me"), UserVoiceState::Muted);

    apply_event(
      &mut roster,
      "clientSelfPropertyUpdated",
      &json!({ "connectionId": 1, "flag": "nickname", "newValue": "Renamed", "oldValue": "Me" }),
    );
    assert!(names(&roster).contains(&"Renamed".to_string()));
  }

  #[test]
  fn connection_lifecycle_tracks_new_and_closed_servers() {
    let mut roster = loaded_roster();

    apply_event(
      &mut roster,
      "connectStatusChanged",
      &json!({ "connectionId": 3, "status": 2, "error": 0, "info": { "clientId": 4 } }),
    );
    apply_event(
      &mut roster,
      "clientMoved",
      &json!({
        "connectionId": 3, "clientId": 4, "oldChannelId": 0, "newChannelId": 1,
        "properties": { "nickname": "OtherServerMe" }
      }),
    );
    assert_eq!(names(&roster), ["Me", "Friend", "OtherServerMe"]);

    apply_event(
      &mut roster,
      "connectStatusChanged",
      &json!({ "connectionId": 1, "status": 0, "error": 0, "info": {} }),
    );
    assert_eq!(names(&roster), ["OtherServerMe"]);
  }

  #[test]
  fn malformed_and_unknown_events_are_ignored() {
    let mut roster = loaded_roster();
    let before = roster.clone();

    assert_eq!(
      apply_event(&mut roster, "talkStatusChanged", &json!({})),
      None
    );
    apply_event(&mut roster, "somethingNew", &json!({ "connectionId": 1 }));
    apply_event(
      &mut roster,
      "talkStatusChanged",
      &json!({ "connectionId": 1, "clientId": 999, "status": 1 }),
    );

    assert_eq!(roster, before);
  }

  #[test]
  fn session_sends_stored_key_and_follows_events() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (auth_tx, auth_rx) = mpsc::channel();

    std::thread::spawn(move || {
      let (stream, _) = listener.accept().unwrap();
      let mut socket = tungstenite::accept(stream).unwrap();
      let auth = socket.read().unwrap().into_text().unwrap();
      auth_tx.send(auth.to_string()).unwrap();

      let reply = json!({
        "type": "auth",
        "payload": auth_payload(),
        "status": { "code": 0, "message": "ok" }
      });
      let talk = json!({
        "type": "talkStatusChanged",
        "payload": { "connectionId": 1, "clientId": 5, "isWhisper": false, "status": 1 }
      });
      socket
        .send(Message::Text(Utf8Bytes::from(reply.to_string())))
        .unwrap();
      socket
        .send(Message::Text(Utf8Bytes::from(talk.to_string())))
        .unwrap();
      while socket.read().is_ok() {}
    });

    let app = test_app(|state| {
      state.config.teamspeak6_enabled = true;
      state.config.teamspeak6_api_key = Some("issued-key".to_string());
    });

    let session_app = app.clone();
    let session =
      std::thread::spawn(move || run_session_at(&session_app, addr).map_err(|e| e.to_string()));

    let auth: Value = serde_json::from_str(&auth_rx.recv().unwrap()).unwrap();
    assert_eq!(auth["payload"]["identifier"], IDENTIFIER);
    assert_eq!(auth["payload"]["content"]["apiKey"], "issued-key");

    wait_for(&app, |state| {
      state
        .teamspeak_users
        .iter()
        .any(|user| user.is_local && user.voice_state == UserVoiceState::Speaking)
    });

    app.update(|state| state.config.teamspeak6_enabled = false);
    session.join().unwrap().unwrap();
  }
}
