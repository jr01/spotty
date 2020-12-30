use std::borrow::Cow;

use std::io::{self, Write};
use std::io::{Read, Seek, SeekFrom};

use std::time::Duration;
use std::time::Instant;

use futures::sync::mpsc::unbounded;
use futures::{future, Async, Future, Poll, Stream};

use tokio_core::reactor::{Core, Timeout};
use tokio_io::IoStream;

use librespot::audio::{AudioDecrypt, AudioFile};

use librespot::core::session::Session;
use librespot::core::spotify_id::SpotifyId;

use librespot::metadata::{AudioItem, FileFormat};

use librespot::playback::config::{Bitrate, PlayerConfig};

/**
 * Timeout and exit after n seconds not having received data (eg. when network cable is unplugged).
 */
const READ_TIMEOUT_SECONDS: u64 = 30;

/**
 * Downloads the track and writes it to stdout.
 */
pub fn download(mut core: Core, session: Session, spotify_id: SpotifyId, player_config: PlayerConfig) {
    let audio = match core.run(AudioItem::get_audio_item(&session, spotify_id)) {
        Ok(audio) => audio,
        Err(_) => {
            eprintln!("Unable to load audio item.");
            return;
        }
    };

    let audio = match find_available_alternative(&audio, &session, &mut core) {
        Some(audio) => audio,
        None => {
            eprintln!("<{}> is not available", audio.uri);
            return;
        }
    };

    // (Most) podcasts seem to support only 96 bit Vorbis, so fall back to it
    let formats = match player_config.bitrate {
        Bitrate::Bitrate96 => [
            FileFormat::OGG_VORBIS_96,
            FileFormat::OGG_VORBIS_160,
            FileFormat::OGG_VORBIS_320,
        ],
        Bitrate::Bitrate160 => [
            FileFormat::OGG_VORBIS_160,
            FileFormat::OGG_VORBIS_96,
            FileFormat::OGG_VORBIS_320,
        ],
        Bitrate::Bitrate320 => [
            FileFormat::OGG_VORBIS_320,
            FileFormat::OGG_VORBIS_160,
            FileFormat::OGG_VORBIS_96,
        ],
    };
    let format = formats
        .iter()
        .find(|format| audio.files.contains_key(format))
        .unwrap();

    let file_id = match audio.files.get(&format) {
        Some(&file_id) => file_id,
        None => {
            eprintln!("<{}> in not available in format {:?}", audio.name, format);
            return;
        }
    };

    let bytes_per_second = match format {
        FileFormat::OGG_VORBIS_96 => 12 * 1024,
        FileFormat::OGG_VORBIS_160 => 20 * 1024,
        FileFormat::OGG_VORBIS_320 => 40 * 1024,
        FileFormat::MP3_256 => 32 * 1024,
        FileFormat::MP3_320 => 40 * 1024,
        FileFormat::MP3_160 => 20 * 1024,
        FileFormat::MP3_96 => 12 * 1024,
        FileFormat::MP3_160_ENC => 20 * 1024,
        FileFormat::MP4_128_DUAL => 16 * 1024,
        FileFormat::OTHER3 => 40 * 1024, // better some high guess than nothing
        FileFormat::AAC_160 => 20 * 1024,
        FileFormat::AAC_320 => 40 * 1024,
        FileFormat::MP4_128 => 16 * 1024,
        FileFormat::OTHER5 => 40 * 1024, // better some high guess than nothing
    };

    let key = session.audio_key().request(spotify_id, file_id);

    let encrypted_file = AudioFile::open(
        &session,
        file_id,
        bytes_per_second,
        true,
    );

    let encrypted_file = match core.run(encrypted_file) {
        Ok(encrypted_file) => encrypted_file,
        Err(_) => {
            eprintln!("Unable to load encrypted file.");
            return;
        }
    };

    let mut stream_loader_controller = encrypted_file.get_stream_loader_controller();
    // Use random access mode since we're not interested in 'streaming' with read-ahead/ping times
    stream_loader_controller.set_random_access_mode();

    let key = match core.run(key) {
        Ok(key) => key,
        Err(_) => {
            eprintln!("Unable to load decryption key");
            return;
        }
    };

    let mut decrypted_file = AudioDecrypt::new(key, encrypted_file);

    // Skip the header.
    const SPOTIFY_TRACK_HEADER_SIZE: u64 = 0xa7;
    decrypted_file.seek(SeekFrom::Start(SPOTIFY_TRACK_HEADER_SIZE)).unwrap();

    // We need to read on a separate thread since .read() blocks/waits for the spawned fetch future.
    let (thread_comm_tx, thread_comm_rx) = unbounded();
    std::thread::spawn(move || {
        // note: we handle all errors and signal our parent to shutdown nicely.
        //       if we don't then fetched files will not be automatically removed from the tempdir (eg. /tmp/.tmp*)
        let mut stdout = io::stdout();

        // Create a small buffer for reading the fetched OGG and writing it to stdout.
        // Note that the capacity of the stdout pipe buffer varies accross systems.
        const BUFFER_SIZE: usize = 64*1024;
        let mut buffer = [0u8; BUFFER_SIZE];

        loop {
            // Here we do a blocking read(...) that waits until data is fetched from the network.
            // This can block indefinetely because of a never ending 1 second poll in librespot's fetch.rs.
            // We work around that by using a read_timeout future and resetting the timeout each time we read data. 
            thread_comm_tx.unbounded_send(Signal::Alive).ok();
            let bytes_read = match decrypted_file.read(&mut buffer) {
                Ok(bytes_read) => bytes_read,
                Err(error) => {
                    eprintln!("Read error {:?}", error);
                    thread_comm_tx.unbounded_send(Signal::Error).ok();
                    break;
                }
            };

            if bytes_read == 0 {
                thread_comm_tx.unbounded_send(Signal::Done).ok();
                break;
            }

            match stdout.write_all(&buffer[0..bytes_read]) {
                Ok(()) => (),
                Err(error) => {
                    eprintln!("Stdout write error {:?}", error);
                    thread_comm_tx.unbounded_send(Signal::Error).ok();
                    break;
                }
            }

            match stdout.flush() {
                Ok(()) => (),
                Err(error) => {
                    eprintln!("Stdout flush error {:?}", error);
                    thread_comm_tx.unbounded_send(Signal::Error).ok();
                    break;
                }
            }
        }
    });

    // Run until we have received all data, ctrl-c pressed, a timeout, or some error occured.
    let future = OggDirectInternal {
        ctrl_c_signal: Box::new(tokio_signal::ctrl_c().flatten_stream()),
        thread_comm_rx: Box::new(thread_comm_rx),
        read_timeout: Timeout::new(Duration::from_secs(READ_TIMEOUT_SECONDS), &core.handle()).unwrap()
    };
    core.run(future).unwrap();
}

fn find_available_alternative<'a>(audio: &'a AudioItem, session: &Session, core: &mut tokio_core::reactor::Core) -> Option<Cow<'a, AudioItem>> {
    if audio.available {
        Some(Cow::Borrowed(audio))
    } else {
        if let Some(alternatives) = &audio.alternatives {
            let alternatives = alternatives
                .iter()
                .map(|alt_id| AudioItem::get_audio_item(session, *alt_id));
            let alternatives = core.run(future::join_all(alternatives)).unwrap();
            alternatives
                .into_iter()
                .find(|alt| alt.available)
                .map(Cow::Owned)
        } else {
            None
        }
    }
}

enum Signal {
    Done,
    Error,
    Alive
}

struct OggDirectInternal {
    ctrl_c_signal: IoStream<()>,
    thread_comm_rx: Box<futures::sync::mpsc::UnboundedReceiver<Signal>>,
    read_timeout: tokio_core::reactor::Timeout
}

impl Future for OggDirectInternal
{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        loop {
            let mut progress = false;

            if let Async::Ready(()) = self.read_timeout.poll().unwrap() {
                // We timedout reading data. Complete the future.
                eprintln!("Timed out after not receiving data for {} seconds!", READ_TIMEOUT_SECONDS);
                return Ok(Async::Ready(()));
            }

            if let Async::Ready(Some(())) = self.ctrl_c_signal.poll().unwrap() {
                // ctrl-c received. Complete the future.
                return Ok(Async::Ready(()));
            }

            if let Async::Ready(signal) = self.thread_comm_rx.poll().unwrap() {
                match signal {
                    Some(res) => match res {
                        Signal::Done | Signal::Error => {
                            // Child thread signals it's done or errored. Complete the future.
                            return Ok(Async::Ready(()));
                        }
                        Signal::Alive => {
                            // Child thread signal's it's alive. Reset the timeout and keep going.
                            progress = true;
                            self.read_timeout.reset(Instant::now() + Duration::from_secs(READ_TIMEOUT_SECONDS));
                        }
                    }
                    None => { }
                }
            }

            if !progress {
                return Ok(Async::NotReady);
            }
        }
    }
}