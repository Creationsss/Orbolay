use std::{
  collections::{BTreeSet, HashMap, VecDeque},
  io::{BufRead, BufReader, ErrorKind, Write},
  net::{SocketAddr, TcpStream},
  path::PathBuf,
  time::{Duration, Instant},
};

use orbolay_core::{app_state::AppHandle, user::VoiceSource};
use orbolay_logging::success;

use super::{
  CONNECT_TIMEOUT, Client, Connection, READ_TIMEOUT, Roster, TsResult, is_enabled, is_timeout,
};

const SOURCE: VoiceSource = VoiceSource::TeamSpeak3;
const PORT: u16 = 25639;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const SYNC_DEBOUNCE: Duration = Duration::from_millis(250);
const SYNC_INTERVAL: Duration = Duration::from_secs(30);
const NOT_CONNECTED: u64 = 1794;

const EVENTS: &[&str] = &[
  "notifytalkstatuschange",
  "notifyclientmoved",
  "notifycliententerview",
  "notifyclientleftview",
  "notifyclientupdated",
  "notifyconnectstatuschange",
];

type Record = HashMap<String, String>;

struct Reply {
  id: u64,
  message: String,
  lines: Vec<String>,
}

impl Reply {
  fn require_ok(self, command: &str) -> TsResult<Self> {
    if self.id == 0 {
      Ok(self)
    } else {
      Err(format!("{command} failed: {} (id {})", self.message, self.id).into())
    }
  }

  fn records(&self) -> Vec<Record> {
    self.lines.iter().flat_map(|l| parse_records(l)).collect()
  }
}

fn unescape(value: &str) -> String {
  let mut output = String::with_capacity(value.len());
  let mut chars = value.chars();

  while let Some(c) = chars.next() {
    if c != '\\' {
      output.push(c);
      continue;
    }

    match chars.next() {
      Some('s') => output.push(' '),
      Some('p') => output.push('|'),
      Some('n') => output.push('\n'),
      Some('r') => output.push('\r'),
      Some('t') => output.push('\t'),
      Some('a') => output.push('\x07'),
      Some('b') => output.push('\x08'),
      Some('f') => output.push('\x0c'),
      Some('v') => output.push('\x0b'),
      Some(other) => output.push(other),
      None => {}
    }
  }

  output
}

fn parse_record(record: &str) -> Record {
  record
    .split_whitespace()
    .filter_map(|field| field.split_once('='))
    .map(|(key, value)| (key.to_string(), unescape(value)))
    .collect()
}

fn parse_records(line: &str) -> Vec<Record> {
  line.split('|').map(parse_record).collect()
}

fn id_field(record: &Record, key: &str) -> Option<u64> {
  record.get(key)?.parse().ok()
}

fn flag_field(record: &Record, key: &str, default: bool) -> bool {
  record.get(key).map_or(default, |value| value == "1")
}

fn apply_fields(client: &mut Client, record: &Record) {
  if let Some(name) = record.get("client_nickname") {
    client.name = name.clone();
  }

  let flags = [
    ("client_flag_talking", &mut client.talking),
    ("client_input_muted", &mut client.input_muted),
    ("client_output_muted", &mut client.output_muted),
    ("client_input_hardware", &mut client.input_hardware),
    ("client_output_hardware", &mut client.output_hardware),
  ];

  for (key, flag) in flags {
    *flag = flag_field(record, key, *flag);
  }
}

fn client_from_record(record: &Record) -> Option<(u64, Client)> {
  let id = id_field(record, "clid")?;
  let mut client = Client {
    channel: id_field(record, "cid")?,
    ..Default::default()
  };
  apply_fields(&mut client, record);

  Some((id, client))
}

fn command_name(command: &str) -> &str {
  command.split(' ').next().unwrap_or(command)
}

struct Query {
  reader: BufReader<TcpStream>,
  writer: TcpStream,
  buffer: Vec<u8>,
  notifications: VecDeque<String>,
}

impl Query {
  fn connect(addr: SocketAddr) -> std::io::Result<Self> {
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(READ_TIMEOUT))?;

    Ok(Self {
      reader: BufReader::new(stream.try_clone()?),
      writer: stream,
      buffer: Vec::new(),
      notifications: VecDeque::new(),
    })
  }

  fn read_line(&mut self) -> std::io::Result<Option<String>> {
    match self.reader.read_until(b'\n', &mut self.buffer) {
      Ok(_) if self.buffer.last() != Some(&b'\n') => Err(ErrorKind::UnexpectedEof.into()),
      Ok(_) => {
        let line = String::from_utf8_lossy(&self.buffer).trim().to_string();
        self.buffer.clear();
        Ok(Some(line))
      }
      Err(e) if is_timeout(&e) => Ok(None),
      Err(e) => Err(e),
    }
  }

  fn command(&mut self, command: &str) -> TsResult<Reply> {
    self.writer.write_all(format!("{command}\n").as_bytes())?;

    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let mut lines = Vec::new();

    while Instant::now() < deadline {
      let Some(line) = self.read_line()? else {
        continue;
      };

      if line.starts_with("notify") {
        self.notifications.push_back(line);
      } else if let Some(status) = line.strip_prefix("error ") {
        let status = parse_record(status);
        return Ok(Reply {
          id: id_field(&status, "id").unwrap_or(u64::MAX),
          message: status.get("msg").cloned().unwrap_or_default(),
          lines,
        });
      } else if !line.is_empty() {
        lines.push(line);
      }
    }

    Err(format!("{} timed out", command_name(command)).into())
  }

  fn run(&mut self, command: &str) -> TsResult<Reply> {
    self.command(command)?.require_ok(command_name(command))
  }

  fn next_notification(&mut self) -> std::io::Result<Option<String>> {
    if let Some(line) = self.notifications.pop_front() {
      return Ok(Some(line));
    }

    Ok(self.read_line()?.filter(|line| line.starts_with("notify")))
  }

  fn read_connection(&mut self, handler: u64) -> TsResult<Option<Connection>> {
    if self.command(&format!("use schandlerid={handler}"))?.id != 0 {
      return Ok(None);
    }

    let whoami = self.command("whoami")?;
    if whoami.id == NOT_CONNECTED {
      return Ok(None);
    }

    let own_client = whoami
      .require_ok("whoami")?
      .records()
      .iter()
      .find_map(|record| id_field(record, "clid"))
      .ok_or("whoami returned no client id")?;

    let clients = self
      .run("clientlist -voice")?
      .records()
      .iter()
      .filter(|record| record.get("client_type").is_none_or(|t| t == "0"))
      .filter_map(client_from_record)
      .collect();

    Ok(Some(Connection {
      own_client,
      clients,
    }))
  }

  fn sync(&mut self, roster: &mut Roster) -> TsResult {
    let handlers: Vec<u64> = self
      .run("serverconnectionhandlerlist")?
      .records()
      .iter()
      .filter_map(|record| id_field(record, "schandlerid"))
      .collect();

    roster.connections.clear();

    for handler in handlers {
      self.refresh(roster, handler)?;
    }

    Ok(())
  }

  fn refresh(&mut self, roster: &mut Roster, handler: u64) -> TsResult {
    match self.read_connection(handler)? {
      Some(connection) => roster.connections.insert(handler, connection),
      None => roster.connections.remove(&handler),
    };

    Ok(())
  }
}

fn clientquery_ini_paths() -> Vec<PathBuf> {
  let mut paths = Vec::new();

  if let Some(dir) = dirs::config_dir() {
    paths.push(dir.join("TS3Client").join("clientquery.ini"));
    paths.push(dir.join("TeamSpeak 3").join("clientquery.ini"));
  }
  if let Some(dir) = dirs::home_dir() {
    paths.push(dir.join(".ts3client").join("clientquery.ini"));
  }

  paths
}

fn parse_api_key(ini: &str) -> Option<String> {
  ini
    .lines()
    .find_map(|line| line.trim().strip_prefix("api_key="))
    .map(|key| key.trim().to_string())
    .filter(|key| !key.is_empty())
}

fn resolve_api_key(app: &AppHandle) -> Option<String> {
  app
    .read(|state| state.config.teamspeak3_api_key.clone())
    .or_else(|| {
      clientquery_ini_paths()
        .iter()
        .find_map(|path| parse_api_key(&std::fs::read_to_string(path).ok()?))
    })
}

fn touches_own_channel(connection: &Connection, record: &Record) -> Option<bool> {
  let own_channel = connection.own_channel()?;
  let client = connection.clients.get(&id_field(record, "clid")?);

  Some(
    client.is_some_and(|client| client.channel == own_channel)
      || ["ctid", "cfid"]
        .iter()
        .any(|key| id_field(record, key) == Some(own_channel)),
  )
}

fn handle_notification(roster: &mut Roster, stale: &mut BTreeSet<u64>, line: &str) -> bool {
  let (name, fields) = line.split_once(' ').unwrap_or((line, ""));
  let record = parse_record(fields);

  let Some(handler) = id_field(&record, "schandlerid") else {
    return false;
  };

  if let Some(client) = id_field(&record, "clid").and_then(|id| roster.client_mut(handler, id)) {
    match name {
      "notifytalkstatuschange" => {
        client.talking = record.get("status").is_some_and(|status| status == "1");
        return true;
      }
      "notifyclientupdated" => {
        apply_fields(client, &record);
        return true;
      }
      _ => {}
    }
  }

  let is_stale = roster
    .connections
    .get(&handler)
    .and_then(|connection| touches_own_channel(connection, &record))
    .unwrap_or(true);

  if is_stale {
    stale.insert(handler);
  }

  false
}

pub(super) fn run_session(app: &AppHandle) -> TsResult {
  let addr = SocketAddr::from(([127, 0, 0, 1], PORT));
  run_session_at(app, addr)
}

fn run_session_at(app: &AppHandle, addr: SocketAddr) -> TsResult {
  let mut query = Query::connect(addr)?;
  let api_key = resolve_api_key(app).ok_or("no ClientQuery API key found")?;

  query.run(&format!("auth apikey={api_key}"))?;

  for event in EVENTS {
    query.run(&format!("clientnotifyregister schandlerid=0 event={event}"))?;
  }

  let mut roster = Roster::new(SOURCE);
  query.sync(&mut roster)?;
  roster.publish(app);
  success!("Connected to {}", SOURCE.display_name());

  let mut stale = BTreeSet::new();
  let mut last_refresh = Instant::now();
  let mut last_sync = Instant::now();

  while is_enabled(app, SOURCE) {
    if let Some(line) = query.next_notification()?
      && handle_notification(&mut roster, &mut stale, &line)
    {
      roster.publish(app);
    }

    if last_sync.elapsed() >= SYNC_INTERVAL {
      query.sync(&mut roster)?;
      roster.publish(app);
      stale.clear();
      last_sync = Instant::now();
    } else if !stale.is_empty() && last_refresh.elapsed() >= SYNC_DEBOUNCE {
      for handler in std::mem::take(&mut stale) {
        query.refresh(&mut roster, handler)?;
      }
      roster.publish(app);
      last_refresh = Instant::now();
    }
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use std::net::TcpListener;

  use orbolay_core::user::UserVoiceState;

  use super::*;
  use crate::teamspeak::tests::{roster_with, test_app, wait_for};

  #[test]
  fn unescapes_query_values() {
    assert_eq!(unescape(r"a\sb\pc\/d\\e"), r"a b|c/d\e");
  }

  #[test]
  fn parses_pipe_separated_records() {
    let records = parse_records(r"clid=1 client_nickname=Big\sBoss|clid=2 client_nickname=x\py");

    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["client_nickname"], "Big Boss");
    assert_eq!(records[1]["clid"], "2");
    assert_eq!(records[1]["client_nickname"], "x|y");
  }

  #[test]
  fn parses_clientlist_voice_record() {
    let records = parse_records(
      "clid=4 cid=9 client_database_id=3 client_nickname=Muh client_type=0 \
       client_flag_talking=1 client_input_muted=0 client_output_muted=0 \
       client_input_hardware=0 client_output_hardware=1",
    );
    let (id, client) = client_from_record(&records[0]).unwrap();

    assert_eq!(id, 4);
    assert_eq!(client.channel, 9);
    assert_eq!(client.name, "Muh");
    assert!(client.talking);
    assert!(!client.input_hardware);
  }

  #[test]
  fn reads_api_key_from_ini() {
    let ini = "[General]\napi_key=AAAA-BBBB\nlogging_enabled=false\n";

    assert_eq!(parse_api_key(ini).as_deref(), Some("AAAA-BBBB"));
    assert_eq!(parse_api_key("[General]\napi_key=\n"), None);
    assert_eq!(parse_api_key(""), None);
  }

  #[test]
  fn only_events_near_own_channel_need_a_refresh() {
    let roster = roster_with(VoiceSource::TeamSpeak3, 5, &[(5, 7), (6, 2)]);
    let touches = |line: &str| touches_own_channel(&roster.connections[&1], &parse_record(line));

    assert_eq!(touches("schandlerid=1 clid=5"), Some(true));
    assert_eq!(touches("schandlerid=1 clid=6 ctid=7"), Some(true));
    assert_eq!(touches("schandlerid=1 clid=99 cfid=7 ctid=0"), Some(true));
    assert_eq!(touches("schandlerid=1 status=disconnected"), None);
    assert_eq!(touches("schandlerid=1 clid=6"), Some(false));
    assert_eq!(touches("schandlerid=1 clid=6 ctid=3"), Some(false));
  }

  #[test]
  fn notifications_update_in_place_or_mark_the_connection_stale() {
    let mut roster = roster_with(VoiceSource::TeamSpeak3, 5, &[(5, 7), (6, 2)]);
    let mut stale = BTreeSet::new();

    assert!(handle_notification(
      &mut roster,
      &mut stale,
      "notifyclientupdated schandlerid=1 clid=5 client_input_muted=1"
    ));
    assert!(roster.connections[&1].clients[&5].input_muted);

    assert!(handle_notification(
      &mut roster,
      &mut stale,
      "notifytalkstatuschange schandlerid=1 status=1 isreceivedwhisper=0 clid=5"
    ));
    assert!(roster.connections[&1].clients[&5].talking);
    assert!(stale.is_empty());

    handle_notification(
      &mut roster,
      &mut stale,
      "notifyclientmoved schandlerid=1 ctid=3 clid=6",
    );
    assert!(stale.is_empty());

    handle_notification(
      &mut roster,
      &mut stale,
      "notifyclientmoved schandlerid=1 ctid=7 clid=6",
    );
    handle_notification(
      &mut roster,
      &mut stale,
      "notifyconnectstatuschange schandlerid=2 status=connected",
    );
    assert_eq!(stale.into_iter().collect::<Vec<_>>(), [1, 2]);
  }

  #[test]
  fn command_name_excludes_arguments() {
    assert_eq!(command_name("auth apikey=SECRET"), "auth");
    assert_eq!(command_name("whoami"), "whoami");
  }

  fn read_command(reader: &mut BufReader<TcpStream>) -> Option<String> {
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    (!line.is_empty()).then(|| line.trim().to_string())
  }

  fn serve(stream: TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    let mut handler = 1;
    let mut lists = 0;

    writer
      .write_all(
        b"TS3 Client\n\rWelcome to the ClientQuery interface\n\rselected schandlerid=1\n\r",
      )
      .unwrap();

    while let Some(command) = read_command(&mut reader) {
      let reply = match command.as_str() {
        "auth apikey=test-key" => "error id=0 msg=ok\n\r".to_string(),
        c if c.starts_with("auth") => "error id=520 msg=invalid\\sclientkey\n\r".to_string(),
        "serverconnectionhandlerlist" => {
          "schandlerid=1|schandlerid=2\n\rerror id=0 msg=ok\n\r".to_string()
        }
        c if c.starts_with("use schandlerid=") => {
          handler = c["use schandlerid=".len()..].parse().unwrap();
          format!("selected schandlerid={handler}\n\rerror id=0 msg=ok\n\r")
        }
        "whoami" if handler == 2 => "error id=1794 msg=not\\sconnected\n\r".to_string(),
        "whoami" => "clid=5 cid=7\n\rerror id=0 msg=ok\n\r".to_string(),
        "clientlist -voice" => {
          lists += 1;
          let (notifications, muted, talking) = if lists == 1 {
            (
              "notifytalkstatuschange schandlerid=1 status=1 isreceivedwhisper=0 clid=6\n\r\
               notifyclientmoved schandlerid=1 ctid=7 clid=9\n\r",
              0,
              0,
            )
          } else {
            ("", 1, 1)
          };

          format!(
            "{notifications}\
             clid=5 cid=7 client_nickname=Me client_type=0 client_input_muted={muted}|\
             clid=6 cid=7 client_nickname=Big\\sBoss client_type=0 client_flag_talking={talking}|\
             clid=8 cid=7 client_nickname=Bot client_type=1|\
             clid=9 cid=2 client_nickname=Elsewhere client_type=0\n\r\
             error id=0 msg=ok\n\r"
          )
        }
        _ => "error id=0 msg=ok\n\r".to_string(),
      };

      writer.write_all(reply.as_bytes()).unwrap();
    }
  }

  fn start_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
      for stream in listener.incoming().flatten() {
        std::thread::spawn(move || serve(stream));
      }
    });
    addr
  }

  #[test]
  fn session_tracks_channel_members_and_talk_events() {
    let addr = start_server();
    let app = test_app(|state| {
      state.config.teamspeak3_enabled = true;
      state.config.teamspeak3_api_key = Some("test-key".to_string());
    });

    let session_app = app.clone();
    let session =
      std::thread::spawn(move || run_session_at(&session_app, addr).map_err(|e| e.to_string()));

    wait_for(&app, |state| {
      state
        .teamspeak_users
        .iter()
        .any(|user| user.is_local && user.voice_state == UserVoiceState::Muted)
    });

    app.read(|state| {
      let names: Vec<_> = state.teamspeak_users.iter().map(|u| &u.name).collect();
      assert_eq!(names, ["Me", "Big Boss"]);
      assert!(state.teamspeak_users[0].is_local);
      assert_eq!(state.teamspeak_users[1].id, "ts3:1:6");
      assert_eq!(
        state.teamspeak_users[1].voice_state,
        UserVoiceState::Speaking
      );
      assert_eq!(state.teamspeak_users[1].source, VoiceSource::TeamSpeak3);
    });

    app.update(|state| state.config.teamspeak3_enabled = false);
    session.join().unwrap().unwrap();
  }

  #[test]
  fn session_reports_rejected_api_key() {
    let addr = start_server();
    let app = test_app(|state| {
      state.config.teamspeak3_api_key = Some("wrong".to_string());
    });

    let error = run_session_at(&app, addr).unwrap_err().to_string();

    assert!(error.contains("invalid clientkey"), "{error}");
  }
}
