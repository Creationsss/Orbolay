#[derive(Clone, Debug, PartialEq, Default)]
pub enum PremiumType {
  #[default]
  None,
  NitroClassic,
  Nitro,
  NitroBasic,
}

impl PremiumType {
  pub fn has_nitro(&self) -> bool {
    !matches!(self, PremiumType::None)
  }
}

impl TryFrom<u64> for PremiumType {
  type Error = String;

  fn try_from(value: u64) -> Result<Self, Self::Error> {
    match value {
      0 => Ok(PremiumType::None),
      1 => Ok(PremiumType::NitroClassic),
      2 => Ok(PremiumType::Nitro),
      3 => Ok(PremiumType::NitroBasic),
      _ => Err(format!("Unknown premium type: {}", value)),
    }
  }
}

#[derive(Clone, Debug, PartialEq)]
pub enum UserVoiceState {
  Speaking,
  NotSpeaking,
  Muted,
  Deafened,
}

impl UserVoiceState {
  pub fn from_flags(deafened: bool, muted: bool, speaking: bool) -> Self {
    if deafened {
      UserVoiceState::Deafened
    } else if muted {
      UserVoiceState::Muted
    } else if speaking {
      UserVoiceState::Speaking
    } else {
      UserVoiceState::NotSpeaking
    }
  }
}

#[derive(Clone, Debug, PartialEq)]
pub struct User {
  pub name: String,
  pub id: String,
  pub avatar: String,
  pub voice_state: UserVoiceState,
  pub streaming: bool,
  pub camera: bool,
  pub volume: f32,
  pub speaking_epoch: u64,
}

use crate::payloads::WsVoiceState;
use crate::payloads::ipc::RpcVoiceState;

impl From<WsVoiceState> for User {
  fn from(val: WsVoiceState) -> Self {
    let voice_state = UserVoiceState::from(&val);

    User {
      name: val.username.unwrap_or("Unknown".to_string()),
      id: val.user_id,
      avatar: val.avatar_url.unwrap_or_default(),
      voice_state,
      streaming: val.streaming.unwrap_or_default(),
      camera: val.camera.unwrap_or_default(),
      volume: 100.,
      speaking_epoch: 0,
    }
  }
}

impl From<WsVoiceState> for UserVoiceState {
  fn from(val: WsVoiceState) -> Self {
    UserVoiceState::from(&val)
  }
}

impl From<&WsVoiceState> for UserVoiceState {
  fn from(val: &WsVoiceState) -> Self {
    UserVoiceState::from_flags(
      val.deaf == Some(true),
      val.mute == Some(true),
      val.speaking == Some(true),
    )
  }
}

impl From<RpcVoiceState> for User {
  fn from(val: RpcVoiceState) -> Self {
    let voice_state = UserVoiceState::from(&val);

    User {
      name: val
        .nick
        .or(val.user.global_name)
        .unwrap_or(val.user.username),
      id: val.user.id,
      avatar: val.user.avatar.unwrap_or_default(),
      voice_state,
      streaming: false,
      camera: false,
      volume: val.volume,
      speaking_epoch: 0,
    }
  }
}

impl From<RpcVoiceState> for UserVoiceState {
  fn from(val: RpcVoiceState) -> Self {
    UserVoiceState::from(&val)
  }
}

impl From<&RpcVoiceState> for UserVoiceState {
  fn from(val: &RpcVoiceState) -> Self {
    UserVoiceState::from_flags(
      val.voice_state.deaf || val.voice_state.self_deaf,
      val.voice_state.mute || val.voice_state.self_mute,
      false,
    )
  }
}
