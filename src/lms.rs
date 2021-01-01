use serde_json::Value;
use sha1::{Digest, Sha1};

use std::str::FromStr;

use tokio_core::reactor::{Handle};

use futures::Future;
use futures::stream::Stream;

use hyper::{Method, Request, Uri, Client};
use hyper::header::{Authorization, ContentLength, ContentType};

use serde::{Deserialize, Deserializer};
use serde::de::{self, Unexpected};

pub enum LMSEvent {
	Stopped,
	Playing { position_ms: u32, play_id: String },
	Paused { position_ms: u32, play_id: String },
}

#[derive(Clone)]
pub struct LMS {
	base_url: Option<String>,
	player_mac: Option<String>,
	auth: Option<String>
}

#[allow(unused)]
impl LMS {
	pub fn new(base_url: Option<String>, player_mac: Option<String>, auth: Option<String>) -> LMS {
		LMS {
			base_url: Some(format!("http://{}/jsonrpc.js", base_url.unwrap_or("localhost:9000".to_string()))),
			player_mac: player_mac,
			auth: auth
		}
	}

	pub fn is_configured(&self) -> bool {
		if self.base_url != None {
			if self.player_mac != None {
				return true;
			}
		}

		return false;
	}

	pub fn send_command(&self, handle: Handle, command: &str) {
		if !self.is_configured() {
			#[cfg(debug_assertions)]
			info!("LMS connection is not configured");
			return;
		}

		#[cfg(debug_assertions)]
		info!("Base URL to talk to LMS: {}", self.base_url.clone().unwrap());

		if let Some(ref base_url) = self.base_url {
			#[cfg(debug_assertions)]
			info!("Player MAC address to control: {}", self.player_mac.clone().unwrap());
			if let Some(ref player_mac) = self.player_mac {

				let client = Client::new(&handle);

				#[cfg(debug_assertions)]
				info!("Command to send to player: {}", command);

				let json = format!(r#"{{"id": 1,"method":"slim.request","params":["{}",{}]}}"#, player_mac, command);
				let uri = Uri::from_str(base_url).unwrap();
				let mut req = Request::new(Method::Post, uri);

				if let Some(ref auth) = self.auth {
					req.headers_mut().set(Authorization(format!("Basic {}", auth).to_owned()));
				}

				req.headers_mut().set_raw("X-Scanner", "1");
				req.headers_mut().set(ContentType::json());
				req.headers_mut().set(ContentLength(json.len() as u64));
				req.set_body(json);

				// ugh... just send that thing and don't care about the rest...
				let post = client.request(req).map(|_| ()).map_err(|_| ());
				handle.spawn(post);
			}
		}
	}

	pub fn poll_state(&mut self, handle: Handle, lms_cmd_tx: futures::sync::mpsc::UnboundedSender<LMSEvent>) {
		let client = Client::new(&handle);

		let post_body = json!({
			"id": 1,
			"method": "slim.request",
			"params": [
				self.player_mac.clone(),
				["status", "-", 1, "tags:gABbehldiqtyrSuoKLNu"]
			]
		});

		if let Some(ref base_url) = self.base_url {
			let uri = Uri::from_str(base_url).unwrap();
			let mut req = Request::new(Method::Post, uri);

			if let Some(ref auth) = self.auth {
				req.headers_mut().set(Authorization(format!("Basic {}", auth).to_owned()));
			}

			req.headers_mut().set_raw("X-Scanner", "1");
			req.headers_mut().set(ContentType::json());

			let json_str = post_body.to_string();
			req.headers_mut().set(ContentLength(json_str.len() as u64));
			req.set_body(json_str);

			let work = client
				.request(req)
				.and_then(|res| {
					// asynchronously concatenate chunks of the body
					res.body().concat2()
				})
				.and_then(move |body| {
					match serde_json::from_slice::<PlayerStatus>(&body) {
						Ok(player_status) => {
							let position_ms: u32 = (player_status.result.time * 1000.0) as u32;
												
							if player_status.result.remote_meta["title"].to_string() != "null" {
								// LMS doesn't have the track_id unfortunately - make one up.
								let play_id =
									format!("{}-{}-{}", 
										player_status.result.remote_meta["artist"].to_string(),
										player_status.result.remote_meta["album"].to_string(),
										player_status.result.remote_meta["title"].to_string()
									);
								let play_id = hex::encode(Sha1::digest(play_id.as_bytes()));

								match player_status.result.mode.as_str() {
									"play" => lms_cmd_tx.unbounded_send(LMSEvent::Playing { position_ms: position_ms, play_id: play_id }).unwrap(),
									"pause" => lms_cmd_tx.unbounded_send(LMSEvent::Paused { position_ms: position_ms, play_id: play_id }).unwrap(),
									"stop" => lms_cmd_tx.unbounded_send(LMSEvent::Stopped).unwrap(),
									&_ => {}
								};
							}
						},
						Err(_) => {
							// ignore for now.
						}
					}
												
					
					Ok(())
				})
				.map_err(|_| ());

			handle.spawn(work);
		}
	}
}



#[derive(Deserialize, Debug)]
pub struct PlayerStatus {
	pub result: PlayerStatusResult,
	pub id: u32, // 1
	pub method: String // slim.request
}
#[derive(Deserialize, Debug)]
pub struct PlayerStatusResult {
	#[serde(deserialize_with = "bool_from_int")]
	pub can_seek: bool,
	pub current_title: String, // why is this empty???
	pub digital_volume_control: u32, // 0 -> boolean???
	pub mode: String, // "play", "stop", "pause"
	#[serde(alias = "mixer volume")]
	pub mixer_volume: u32,
    #[serde(deserialize_with = "bool_from_int")]
    pub player_connected: bool,
    pub playlist_cur_index: String,
    #[serde(alias = "playlist mode")]
    pub playlist_mode: String, // "off"
    #[serde(alias = "playlist repeat", deserialize_with = "bool_from_int")]
    pub playlist_repeat: bool,
    #[serde(alias = "playlist shuffle", deserialize_with = "bool_from_int")]
    pub playlist_shuffle: bool,
    pub playlist_timestamp: f64,
    pub playlist_tracks: u32, // number of tracks in the playlist.
    pub power: u32,
    #[serde(deserialize_with = "bool_from_int")]
    pub remote: bool, // 1
    #[serde(alias = "remoteMeta")]
    pub remote_meta: Value,
    pub seq_no: String, // ????
    pub signalstrength: u32, // 0
    pub time: f64, // position.
}

fn bool_from_int<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    match u8::deserialize(deserializer)? {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(de::Error::invalid_value(
            Unexpected::Unsigned(other as u64),
            &"zero or one",
        )),
    }
}