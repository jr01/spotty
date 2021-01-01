use std::time::Instant;
use std::time::Duration;
use tokio_core::reactor::Timeout;
use tokio_core::reactor::{Handle};
use futures::{Async, Future, Poll, Stream};

use librespot::core::spotify_id::SpotifyId;
use librespot::core::util::SeqGenerator;

use librespot::playback::player::{PlayerEvent, SinkEventCallback};

use crate::lms::{LMS, LMSEvent};

// ToDo: modify librespot so it better separates player from spirc - then all this shouldn't be needed.
#[allow(dead_code)]
pub struct Player {
    commands: Option<futures::sync::mpsc::UnboundedSender<PlayerCommand>>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
    play_request_id_generator: SeqGenerator<u64>,
}

#[allow(dead_code)]
enum PlayerCommand {
    Load {
        track_id: SpotifyId,
        play_request_id: u64,
        play: bool,
        position_ms: u32,
    },
    Preload {
        track_id: SpotifyId,
    },
    Play,
    Pause,
    Stop,
    Seek(u32),
    AddEventSender(futures::sync::mpsc::UnboundedSender<PlayerEvent>),
    SetSinkEventCallback(Option<SinkEventCallback>),
    EmitVolumeSetEvent(u16),
}

impl Player {
    pub fn new(lms: LMS, handle: Handle) -> Player {
        let (cmd_tx, cmd_rx) = futures::sync::mpsc::unbounded();

        let (lms_cmd_tx, lms_cmd_rx) = futures::sync::mpsc::unbounded();

        let internal = PlayerInternal {
            cmd_rx: cmd_rx,
            event_senders: Vec::new(),
            lms: lms,
            handle: handle.clone(),
            track_state: TrackState::new(),
            pinger: Timeout::new(Duration::from_secs(1), &handle).unwrap(),
            lms_cmd_rx:lms_cmd_rx,
            lms_cmd_tx:lms_cmd_tx 
        };

        handle.spawn(internal);

        Player {
            commands: Some(cmd_tx),
            thread_handle: None,
            play_request_id_generator: SeqGenerator::new(0)
        }
    }
}

struct PlayerInternal {
    cmd_rx: futures::sync::mpsc::UnboundedReceiver<PlayerCommand>,
    event_senders: Vec<futures::sync::mpsc::UnboundedSender<PlayerEvent>>,
    lms: LMS,
    handle: Handle,
    track_state: TrackState,
    pinger: tokio_core::reactor::Timeout,
    lms_cmd_rx: futures::sync::mpsc::UnboundedReceiver<LMSEvent>,
    lms_cmd_tx: futures::sync::mpsc::UnboundedSender<LMSEvent>
}

struct TrackState {
    load_track_id: Option<SpotifyId>,
    current_play_request_id: u64,
    load_play_request_id: u64,
    current_track_id: Option<SpotifyId>,
    current_play_id: String,
}

impl TrackState {
    pub fn new() -> TrackState {
        TrackState {
            load_track_id: None,
            load_play_request_id: 0,
            current_play_request_id: 0,
            current_track_id: None,
            current_play_id: String::from(""),
        }
    }
}

impl PlayerInternal {
    fn handle_command(&mut self, cmd: PlayerCommand) {
        match cmd {
            PlayerCommand::Load {
                track_id,
                play_request_id,
                play,
                position_ms,
            } => self.handle_command_load(track_id, play_request_id, play, position_ms),
            PlayerCommand::Preload { track_id } => self.handle_command_preload(track_id),
            PlayerCommand::Seek(position_ms) => self.handle_command_seek(position_ms),
            PlayerCommand::Play => self.handle_command_play(),
            PlayerCommand::Pause => self.handle_command_pause(),
            PlayerCommand::Stop => self.handle_command_stop(),
            PlayerCommand::AddEventSender(sender) => self.event_senders.push(sender),
            PlayerCommand::SetSinkEventCallback(callback) => self.handle_command_setsinkcallback(callback),
            PlayerCommand::EmitVolumeSetEvent(volume) => self.handle_command_volume(volume),
        }
    }

    fn handle_command_setsinkcallback(&mut self, _callback: Option<SinkEventCallback>) {
        // command is never send.
    }

    fn handle_command_preload(&mut self, _track_id: SpotifyId) {
        // command is never send???
    }

    fn handle_command_load(
        &mut self,
        track_id: SpotifyId,
        play_request_id: u64,
        play: bool,
        position_ms: u32,
    ) {
        #[cfg(debug_assertions)]
        debug!("LOAD: {}, {}, {}", play, position_ms, play_request_id);
        // This event is sent:
        // - when a Spotify client connects - it then has play=false
        // - on 'next track' - it then has play=true
        // - on 'prev' - it has play=true
        // - when the end of the track is reached and we can load up a next track.
        let mut load_new_track = false;
        match self.track_state.current_track_id {
            Some(current_track_id) => {
                if current_track_id != track_id {
                    load_new_track = true;
                }
            },
            None => {
                load_new_track = true;
            }
        }
        
        if load_new_track {
            self.track_state.load_track_id = Some(track_id);
            self.track_state.load_play_request_id = play_request_id;

            // Notify that we are delayed.
            self.send_event(PlayerEvent::Loading {
                play_request_id: play_request_id,
                track_id: track_id, 
                position_ms: position_ms
            });
        }

        if play {
            // Tell LMS to start playing the track.
            let command = format!(r#"["spottyconnect","start","{}"]"#, track_id.to_base62());
            self.lms.send_command(self.handle.clone(), &command);
        }
    }

    fn handle_command_play(&mut self) {
        // When pressing 'play' in Spotify - send start command without a track to resume playback in LMS.
        self.lms.send_command(self.handle.clone(), r#"["spottyconnect","start"]"#);
    }

    fn handle_command_pause(&self) {
        // When pressing 'pause' in Spotify - send stop command which is interpreted as pause in LMS.
        self.lms.send_command(self.handle.clone(), r#"["spottyconnect","stop"]"#);
    }

    fn handle_command_seek(&self, _position_ms: u32) {
        // When seeking in Spotify - send the change command to LMS and let LMS figure it out.
        self.lms.send_command(self.handle.clone(), r#"["spottyconnect","change"]"#);
    }

    fn handle_command_stop(&self) {
        self.lms.send_command(self.handle.clone(), r#"["spottyconnect","stop"]"#);
    }

    fn handle_command_volume(&mut self, _volume: u16) {
        // Volume is ignored in Connect.pm - changing volume in LMS sends it, but other way around is not working.
        // let command = format!(r#"["spottyconnect","volume",{}]"#, volume);
        // self.lms.send_command(self.handle.clone(), &command);
    }

    fn send_event(&mut self, event: PlayerEvent) {
        let mut index = 0;
        while index < self.event_senders.len() {
            match self.event_senders[index].unbounded_send(event.clone()) {
                Ok(_) => index += 1,
                Err(_) => {
                    self.event_senders.remove(index);
                }
            }
        }
    }

    fn handle_lms_event(&mut self, event: LMSEvent) {
        match event {
            LMSEvent::Playing { position_ms, play_id } => {
                if play_id != self.track_state.current_play_id {
                    if let Some(_load_track_id) = self.track_state.load_track_id {
                        // This is the first time we receive an event after the track got loaded.
                        // From now on we should send progress events for this track.
                        #[cfg(debug_assertions)]
                        debug!("Loading {} - {}", play_id, position_ms);

                        self.track_state.current_play_id = play_id.clone();
                        self.track_state.current_track_id = self.track_state.load_track_id;
                        self.track_state.current_play_request_id = self.track_state.load_play_request_id;
                        self.track_state.load_track_id = None;
                        self.track_state.load_play_request_id = 0;
                    }
                }

                if let Some(track_id) = self.track_state.current_track_id {
                    if self.track_state.current_play_id == play_id {
                        #[cfg(debug_assertions)]
                        debug!("Playing {} - {}", play_id, position_ms);
                        // Received an event for the currently playing track.
                        let event = PlayerEvent::Playing {
                            track_id: track_id,
                            play_request_id: self.track_state.current_play_request_id,
                            position_ms: position_ms,
                            duration_ms: 0,
                        };

                        self.send_event(event);
                    }
                }
            },
            LMSEvent::Stopped => {
            },
            LMSEvent::Paused { position_ms, play_id } => {

                if play_id != self.track_state.current_play_id {
                    if let Some(_load_track_id) = self.track_state.load_track_id {
                        // This is the first time we receive an event after the track got loaded.
                        // From now on we should send progress events for this track.
                        self.track_state.current_play_id = play_id.clone();
                        self.track_state.current_track_id = self.track_state.load_track_id;
                        self.track_state.current_play_request_id = self.track_state.load_play_request_id;
                        self.track_state.load_track_id = None;
                        self.track_state.load_play_request_id = 0;
                    }
                }

                if let Some(track_id) = self.track_state.current_track_id {
                    if self.track_state.current_play_id == play_id {
                        // Received an event for the currently playing track.
                        let event = PlayerEvent::Paused {
                            track_id: track_id,
                            play_request_id: self.track_state.current_play_request_id,
                            position_ms: position_ms,
                            duration_ms: 0,
                        };

                        self.send_event(event);
                    }
                }
            }
        }

    }
}

impl Future for PlayerInternal {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        loop {
            let mut progress = false;

            match self.cmd_rx.poll() {
                // process commands that were sent to us
                Ok(Async::Ready(None)) => return Ok(Async::Ready(())), // client has disconnected - shut down.
                Ok(Async::Ready(Some(cmd))) => {
                    progress = true;
                    self.handle_command(cmd);
                }
                Ok(Async::NotReady) => (),
                Err(_) => (),
            };

            if let Async::Ready(()) = self.pinger.poll().unwrap() {
                progress = true;

                // ToDO: this is a bit ugly, polling should be an LMS implementation detail and in the top of this file we should subscribe to it's events.
                // ToDO: also investigate subscription option in LMS api.
                self.lms.poll_state(self.handle.clone(), self.lms_cmd_tx.clone());

                self.pinger.reset(Instant::now() + Duration::from_secs(1));
            }

            match self.lms_cmd_rx.poll() {
                // process LMS events
                Ok(Async::Ready(None)) => return Ok(Async::Ready(())),
                Ok(Async::Ready(Some(event))) => {
                    self.handle_lms_event(event);
                    progress = true;
                },
                Ok(Async::NotReady) => (),
                Err(_) => (),
            }

            if !progress {
                return Ok(Async::NotReady);
            }
        }
    }
}
