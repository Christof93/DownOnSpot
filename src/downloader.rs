use async_std::channel::{bounded, Receiver, Sender};
use async_stream::try_stream;
// use chrono::NaiveDate;
use futures::stream::FuturesUnordered;
use futures::{pin_mut, select, FutureExt, Stream, StreamExt, Future};
use librespot::audio::{AudioDecrypt, AudioFile};
use librespot::core::audio_key::AudioKey;
use librespot::core::session::Session;
use librespot::core::spotify_id::SpotifyId;
use librespot::metadata::audio::{AudioFileFormat, AudioFiles};
use librespot::metadata::{Metadata, Track};
// use sanitize_filename::sanitize;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use tokio::fs::File;
use tokio::time::Instant;
use tokio::io::AsyncWriteExt;
use std::fs;

use crate::converter::AudioConverter;
use crate::error::SpotifyError;
use crate::spotify::{Spotify, SpotifyItem};
// use crate::tag::{Field, TagWrap};

/// Wrapper for use with UI
#[derive(Debug, Clone)]
pub struct Downloader {
	rx: Receiver<Response>,
	tx: Sender<Message>,

	spotify: Spotify,
}
impl Downloader {
	/// Create new instance
	pub fn new(config: DownloaderConfig, spotify: Spotify) -> Downloader {
		let (tx_0, rx_0) = bounded(1);
		let (tx_1, rx_1) = bounded(1);

		let tx_clone = tx_1.clone();
		let spotify_clone = spotify.clone();
		tokio::spawn(async move {
			communication_thread(config, spotify_clone, rx_1, tx_0, tx_clone).await
		});
		Downloader {
			rx: rx_0,
			tx: tx_1,
			spotify,
		}
	}
	/// Add item to download queue
	pub async fn add_to_queue(&self, download: Download) {
		self.tx
			.send(Message::AddToQueue(vec![download]))
			.await
			.unwrap();
	}

	/// Add multiple items to queue
	pub async fn add_to_queue_multiple(&self, downloads: Vec<Download>) {
		self.tx.send(Message::AddToQueue(downloads)).await.unwrap();
	}

	/// handle input, either link or search
	pub async fn handle_input(
		&self,
		input: &str,
	) -> Result<Option<Vec<SearchResult>>, SpotifyError> {
		if input.starts_with("-f") || input.starts_with("--file") {
			if let Some(file_path)=input.split_whitespace().nth(1) {
				println!("loading tracks from: {}", file_path);
				let contents = fs::read_to_string(file_path)
        			.expect("Should have been able to read the file");
				let ids = contents.lines();
				let mut queue:Vec<Download> = vec![];
				for id in ids {
					let uri = format!("spotify:track:{id}");
					let uri = Spotify::parse_uri(&uri)?;
					let item = self.spotify.resolve_uri(&uri).await?;
					if let SpotifyItem::Track(track) = item {
						queue.push(track.into());
					}
				}
				self.add_to_queue_multiple(queue).await;
				Ok(None)
			} else {
				println!("There is no second element");
				Ok(None)
			}
		} else if let Ok(uri) = Spotify::parse_uri(input) {
			if let Err(e) = self.add_uri(&uri).await {
				return Err(e);
			}
			Ok(None)
		} else {
			let results: Vec<SearchResult> = self
				.spotify
				.search(input)
				.await?
				.into_iter()
				.map(SearchResult::from)
				.collect();

			Ok(Some(results))
		}
	}

	/// Add URL or URI to queue
	pub async fn add_uri(&self, uri: &str) -> Result<(), SpotifyError> {
		let uri = Spotify::parse_uri(uri)?;
		let item = self.spotify.resolve_uri(&uri).await?;
		match item {
			SpotifyItem::Track(t) => self.add_to_queue(t.into()).await,
			SpotifyItem::Album(a) => {
				let tracks = self.spotify.full_album(&a.id).await?;
				let queue: Vec<Download> = tracks.into_iter().map(|t| t.into()).collect();
				self.add_to_queue_multiple(queue).await;
			}
			SpotifyItem::Playlist(p) => {
				let tracks = self.spotify.full_playlist(&p.id).await?;
				let queue: Vec<Download> = tracks.into_iter().map(|t| t.into()).collect();
				self.add_to_queue_multiple(queue).await;
			}
			SpotifyItem::Artist(a) => {
				let tracks = self.spotify.full_artist(&a.id).await?;
				let queue: Vec<Download> = tracks.into_iter().map(|t| t.into()).collect();
				self.add_to_queue_multiple(queue).await;
			}

			// Unsupported
			SpotifyItem::Other(u) => {
				error!("Unsupported URI: {}", u);
				return Err(SpotifyError::Unavailable);
			}
		};
		Ok(())
	}

	/// Get all downloads
	pub async fn get_downloads(&self) -> Vec<Download> {
		self.tx.send(Message::GetDownloads).await.unwrap();
		let Response::Downloads(d) = self.rx.recv().await.unwrap();
		d
	}
}

async fn communication_thread(
	config: DownloaderConfig,
	spotify: Spotify,
	rx: Receiver<Message>,
	tx: Sender<Response>,
	self_tx: Sender<Message>,
) {
	// Downloader
	let downloader = DownloaderInternal::new(spotify.clone(), self_tx.clone());
	let downloader_tx = downloader.tx.clone();
	tokio::spawn(async move {
		downloader.download_loop().await;
	});
	let mut waiting_for_job = false;
	let mut queue: Vec<Download> = vec![];

	// Receive messages
	while let Ok(msg) = rx.recv().await {
		match msg {
			// Send job to worker thread
			Message::GetJob => {
				if let Some(d) = queue.iter_mut().find(|i| i.state == DownloadState::None) {
					d.state = DownloadState::Lock;
					downloader_tx
						.send(DownloaderMessage::Job(d.clone().into(), config.clone()))
						.await
						.unwrap();
					waiting_for_job = false;
				} else {
					waiting_for_job = true;
				}
			}
			// Update state of download
			Message::UpdateState(id, state) => {
				let i = queue.iter().position(|i| i.id == id).unwrap();
				queue[i].state = state.clone();
				if state == DownloadState::Done {
					queue.remove(i);
				}
			}
			Message::AddToQueue(download) => {
				// Assign new IDs and reset state
				let mut id = queue.iter().map(|i| i.id).max().unwrap_or(0);
				let downloads: Vec<Download> = download
					.into_iter()
					.map(|mut d| {
						d.id = id;
						d.state = DownloadState::None;
						id += 1;
						d
					})
					.collect();
				queue.extend(downloads);
				// Update worker threads if locked
				if waiting_for_job {
					let d = queue
						.iter_mut()
						.find(|i| i.state == DownloadState::None)
						.unwrap();
					d.state = DownloadState::Lock;
					downloader_tx
						.send(DownloaderMessage::Job(d.clone().into(), config.clone()))
						.await
						.unwrap();
					waiting_for_job = false;
				}
			}
			Message::GetDownloads => {
				tx.send(Response::Downloads(queue.clone())).await.ok();
			}
		}
	}
}

/// Spotify downloader
pub struct DownloaderInternal {
	spotify: Spotify,
	pub tx: Sender<DownloaderMessage>,
	rx: Receiver<DownloaderMessage>,
	event_tx: Sender<Message>,
}

pub enum DownloaderMessage {
	Job(DownloadJob, DownloaderConfig),
}

impl DownloaderInternal {
	/// Create new instance
	pub fn new(spotify: Spotify, event_tx: Sender<Message>) -> DownloaderInternal {
		let (tx, rx) = bounded(1);
		DownloaderInternal {
			spotify,
			tx,
			rx,
			event_tx,
		}
	}

	/// Downloader loop
	pub async fn download_loop(&self) {
		let mut queue = vec![];
		let mut tasks = FuturesUnordered::new();
		let mut job_future = Box::pin(self.get_job()).fuse();

		loop {
			select! {
				job = job_future => {
					if let Some((job, config)) = job {
						if tasks.len() < config.concurrent_downloads {
							tasks.push(self.download_job_wrapper(job.clone(), config).boxed())
						} else {
							queue.push((job, config));
						}
					}
					job_future = Box::pin(self.get_job()).fuse();
				},
				// Task finished
				() = tasks.select_next_some() => {
					if let Some((job, config)) = queue.first() {
						tasks.push(self.download_job_wrapper(job.clone(), config.clone()).boxed());
						queue.remove(0);
					}
				}
			};
		}
	}

	// Get job from parent
	async fn get_job(&self) -> Option<(DownloadJob, DownloaderConfig)> {
		self.event_tx.send(Message::GetJob).await.unwrap();
		match self.rx.recv().await.ok()? {
			DownloaderMessage::Job(job, config) => Some((job, config)),
		}
	}

	/// Wrapper for download_job for error handling
	async fn download_job_wrapper(&self, job: DownloadJob, config: DownloaderConfig) {
		let track_id = job.track_id.clone();
		let id = job.id;
		match self.download_job(job, config).await {
			Ok(_) => {}
			Err(e) => {
				error!("Download job for track {} failed. {}", track_id, e);
				self.event_tx
					.send(Message::UpdateState(
						id,
						DownloadState::Error(e.to_string()),
					))
					.await
					.unwrap();
			}
		}
	}

	// Wrapper for downloading and tagging
	async fn download_job(
		&self,
		job: DownloadJob,
		config: DownloaderConfig,
	) -> Result<(), SpotifyError> {
		// Fetch metadata
		// let track = self
		// 	.spotify
		// 	.spotify
		// 	.tracks()
		// 	.get_track(&job.track_id, None)
		// 	.await?
		// 	.data;
		// let album = self
		// 	.spotify
		// 	.spotify
		// 	.albums()
		// 	.get_album(&track.album.id.ok_or(SpotifyError::Unavailable)?, None)
		// 	.await?
		// 	.data;

		let tags: Vec<(&str, String)> = vec![
			// ("%title%", sanitize(&track.name)),
			// (
			// 	"%artist%",
			// 	sanitize(
			// 		&track
			// 			.artists
			// 			.iter()
			// 			.map(|a| a.name.as_str())
			// 			.collect::<Vec<&str>>()
			// 			.first()
			// 			.unwrap_or(&""),
			// 	),
			// ),
			// (
			// 	"%artists%",
			// 	sanitize(
			// 		&track
			// 			.artists
			// 			.iter()
			// 			.map(|a| a.name.as_str())
			// 			.collect::<Vec<&str>>()
			// 			.join(", "),
			// 	),
			// ),
			// ("%track%", track.track_number.to_string()),
			// ("%0track%", format!("{:02}", track.track_number)),
			// ("%disc%", track.disc_number.to_string()),
			// ("%0disc%", format!("{:02}", track.disc_number)),
			("%id%", job.track_id.to_string()),
			// ("%album%", sanitize(&track.album.name)),
			// (
			// 	"%albumArtist%",
			// 	sanitize(
			// 		&track
			// 			.album
			// 			.artists
			// 			.iter()
			// 			.map(|a| a.name.as_str())
			// 			.collect::<Vec<&str>>()
			// 			.first()
			// 			.unwrap_or(&""),
			// 	),
			// ),
			// (
			// 	"%albumArtists%",
			// 	sanitize(
			// 		&track
			// 			.album
			// 			.artists
			// 			.iter()
			// 			.map(|a| a.name.as_str())
			// 			.collect::<Vec<&str>>()
			// 			.join(", "),
			// 	),
			// ),
		];

		// filename while streaming
		let mut filename_template = config.tmp_filename_template.clone();
		let mut path_template = config.tmp_path.clone();
		// filename upon finish downloading
		let mut final_filename_template = config.filename_template.clone();
		let mut final_path_template = config.path.clone();
		for (tag, value) in tags {
			filename_template = filename_template.replace(tag, &value);
			path_template = path_template.replace(tag, &value);
			final_filename_template = final_filename_template.replace(tag, &value);
			final_path_template = final_path_template.replace(tag, &value);
		}
		let final_path = Path::new(&final_path_template).join(&final_filename_template);
		let path = Path::new(&path_template).join(&filename_template);

		tokio::fs::create_dir_all(path.parent().unwrap()).await?;
		tokio::fs::create_dir_all(final_path.parent().unwrap()).await?;

		// Download
		let (streaming_path, final_path, _format) = DownloaderInternal::download_track(
			&self.spotify.session,
			&job.track_id,
			&path,
			&final_path,
			config.clone(),
			self.event_tx.clone(),
			job.id,
		)
		.await?;
		// Post processing
		self.event_tx
			.send(Message::UpdateState(job.id, DownloadState::Post))
			.await
			.ok();

		// // Download cover
		// let mut cover = None;
		// if let Some(image) = track.album.images.first() {
		// 	match DownloaderInternal::download_cover(&image.url).await {
		// 		Ok(c) => cover = Some(c),
		// 		Err(e) => warn!("Failed downloading cover! {}", e),
		// 	}
		// }

		// let tags = vec![
		// 	(Field::Title, vec![track.name.to_string()]),
		// 	(Field::Album, vec![track.album.name.to_string()]),
		// 	(
		// 		Field::Artist,
		// 		track
		// 			.artists
		// 			.iter()
		// 			.map(|a| a.name.to_string())
		// 			.collect::<Vec<String>>(),
		// 	),
		// 	(
		// 		Field::AlbumArtist,
		// 		track
		// 			.album
		// 			.artists
		// 			.iter()
		// 			.map(|a| a.name.to_string())
		// 			.collect::<Vec<String>>(),
		// 	),
		// 	(Field::TrackNumber, vec![track.track_number.to_string()]),
		// 	(Field::DiscNumber, vec![track.disc_number.to_string()]),
		// 	(Field::Genre, album.genres.clone()),
		// 	(Field::Label, vec![album.label.to_string()]),
		// ];
		// let date = album.release_date;
		// // Write tags
		// let config = config.clone();
		// tokio::task::spawn_blocking(move || {
		// 	DownloaderInternal::write_tags(path, format, tags, date, cover, config)
		// })
		// .await??;

		// Done
		self.event_tx
			.send(Message::UpdateState(job.id, DownloadState::Done))
			.await
			.ok();

		// rename file 
		tokio::fs::rename(&streaming_path, &final_path).await?;
		Ok(())
	}

	// /// Download cover, returns mime and data
	// async fn download_cover(url: &str) -> Result<(String, Vec<u8>), SpotifyError> {
	// 	let res = reqwest::get(url).await?;
	// 	let mime = res
	// 		.headers()
	// 		.get("content-type")
	// 		.ok_or_else(|| SpotifyError::Error("Missing cover mime!".into()))?
	// 		.to_str()
	// 		.unwrap()
	// 		.to_string();
	// 	let data = res.bytes().await?.to_vec();
	// 	Ok((mime, data))
	// }

	// /// Write tags to file ( BLOCKING )
	// fn write_tags(
	// 	path: impl AsRef<Path>,
	// 	format: AudioFormat,
	// 	tags: Vec<(Field, Vec<String>)>,
	// 	date: NaiveDate,
	// 	cover: Option<(String, Vec<u8>)>,
	// 	config: DownloaderConfig,
	// ) -> Result<(), SpotifyError> {
	// 	let mut tag_wrap = TagWrap::new(path, format)?;
	// 	// Format specific
	// 	if let TagWrap::Id3(id3) = &mut tag_wrap {
	// 		id3.use_id3_v24(config.id3v24)
	// 	}

	// 	let tag = tag_wrap.get_tag();
	// 	tag.set_separator(&config.separator);
	// 	for (field, value) in tags {
	// 		tag.set_field(field, value);
	// 	}
	// 	tag.set_release_date(date);
	// 	// Cover
	// 	if let Some((mime, data)) = cover {
	// 		tag.add_cover(&mime, data);
	// 	}
	// 	tag.save()?;
	// 	Ok(())
	// }

	//Pin<Box<dyn Future<Output = std::result::Result<rtsp_types::Response<Body>, ClientActionError>> + Send>> 
	fn find_alternative(session: &Session, track: Track)
	-> Pin<Box<dyn Future<Output = std::result::Result<AudioFiles, SpotifyError>> + Send +'_>> {
		async move {
			for alt in &track.alternatives.0 {
				let t = Track::get(&session, &alt).await?;
				if !t.files.is_empty() {
					return Ok(t.files);
				}
				else {
					return Self::find_alternative(session, t).await;
				}
			}
			return Err(SpotifyError::Unavailable);
		}.boxed()
	}

	/// Download track by id
	async fn download_track(
		session: &Session,
		id: &str,
		path: impl AsRef<Path>,
		final_path: impl AsRef<Path>,
		config: DownloaderConfig,
		tx: Sender<Message>,
		job_id: i64,
	) -> Result<(PathBuf, PathBuf, AudioFormat), SpotifyError> {
		let id = SpotifyId::from_base62(id)?;
		let track = Track::get(session, &id).await?;
		let track_files: AudioFiles;
		let now = Instant::now();
		let mut time_elapsed: u64;

		if track.files.is_empty() {
			track_files = DownloaderInternal::find_alternative(session, track.clone()).await?;
		}
		else {
			track_files = track.files;
		}

		// Quality fallback
		let mut quality = config.quality;
		let (mut file_id, mut file_format) = (None, None);
		'outer: loop {
			for format in quality.get_file_formats() {
				if let Some(f) = track_files.get(&format) {
					info!("{} Using {:?} format.", id.to_base62().unwrap(), format);
					file_id = Some(f);
					file_format = Some(format);
					break 'outer;
				}
			}
			// Fallback to worse quality
			match quality.fallback() {
				Some(q) => quality = q,
				None => break,
			}
			warn!("{} Falling back to: {:?}", id.to_base62().unwrap(), quality);
		}

		let file_id = file_id.ok_or(SpotifyError::Unavailable)?;
		let file_format = file_format.unwrap();

		// Path with extension
		let mut audio_format: AudioFormat = file_format.into();
		let path = format!(
			"{}.{}",
			path.as_ref().to_str().unwrap(),
			match config.convert_to_mp3 {
				true => "mp3".to_string(),
				false => audio_format.extension(),
			}
		);
		let final_path = format!(
			"{}.{}",
			final_path.as_ref().to_str().unwrap(),
			match config.convert_to_mp3 {
				true => "mp3".to_string(),
				false => audio_format.extension(),
			}
		);
		let path = Path::new(&path).to_owned();
		let final_path = Path::new(&final_path).to_owned();

		// Don't download if we are skipping and the path exists.
		if config.skip_existing && final_path.is_file() {
			return Err(SpotifyError::AlreadyDownloaded);
		}


		let path_clone = path.clone();

		let key = session.audio_key().request(track.id, *file_id).await?;
		let encrypted = AudioFile::open(session, *file_id, 1024 * 1024).await?;
		let size = encrypted.get_stream_loader_controller()?.len();
		// Download
		let s = match config.convert_to_mp3 {
			true => {
				let s = DownloaderInternal::download_track_convert_stream(
					path_clone,
					encrypted,
					key,
					audio_format.clone(),
					quality,
				)
				.boxed();
				audio_format = AudioFormat::Mp3;
				s
			}
			false => DownloaderInternal::download_track_stream(path_clone, encrypted, key).boxed(),
		};
		pin_mut!(s);
		// Read progress
		let mut read = 0;
		while let Some(result) = s.next().await {
			match result {
				Ok(r) => {
					time_elapsed = now.elapsed().as_secs();
					if time_elapsed as u16 >= config.timeout {
						return Err(SpotifyError::Timeout);
					}
					else {
						read += r;
						tx.send(Message::UpdateState(
							job_id,
							DownloadState::Downloading(read, size, time_elapsed),
						))
						.await
						.ok();
					}
				}
				Err(e) => {
					tokio::fs::remove_file(path).await.ok();
					return Err(e);
				}
			}
		}

		info!("Done downloading: {}", track.id.to_base62().unwrap());
		Ok((path, final_path, audio_format))
	}

	fn download_track_stream(
		path: impl AsRef<Path>,
		encrypted: AudioFile,
		key: AudioKey,
	) -> impl Stream<Item = Result<usize, SpotifyError>> {
		try_stream! {
			let mut file = File::create(path).await?;
			let mut decrypted = AudioDecrypt::new(Some(key), encrypted);
			// Skip (i guess encrypted shit)
			let mut skip: [u8; 0xa7] = [0; 0xa7];
			let mut decrypted = tokio::task::spawn_blocking(move || {
				match decrypted.read_exact(&mut skip) {
					Ok(_) => Ok(decrypted),
					Err(e) => Err(e)
				}
			}).await??;
			// Custom reader loop for decrypting
			loop {
				// Blocking reader
				let (d, read, buf) = tokio::task::spawn_blocking(move || {
					let mut buf = vec![0; 1024 * 64];
					match decrypted.read(&mut buf) {
						Ok(r) => Ok((decrypted, r, buf)),
						Err(e) => Err(e)
					}
				}).await??;
				decrypted = d;
				if read == 0 {
					break;
				}
				file.write_all(&buf[0..read]).await?;
				yield read;
			}
		}
	}
	/// Download and convert to MP3
	fn download_track_convert_stream(
		path: impl AsRef<Path>,
		encrypted: AudioFile,
		key: AudioKey,
		format: AudioFormat,
		quality: Quality,
	) -> impl Stream<Item = Result<usize, SpotifyError>> {
		try_stream! {
			let mut file = File::create(path).await?;
			let mut decrypted = AudioDecrypt::new(Some(key), encrypted);
			// Skip (i guess encrypted shit)
			let mut skip: [u8; 0xa7] = [0; 0xa7];
			let decrypted = tokio::task::spawn_blocking(move || {
				match decrypted.read_exact(&mut skip) {
					Ok(_) => Ok(decrypted),
					Err(e) => Err(e)
				}
			}).await??;
			// Convertor
			let mut decrypted = tokio::task::spawn_blocking(move || {
				AudioConverter::new(Box::new(decrypted), format, quality)
			}).await??;

			// Custom reader loop for decrypting
			loop {
				// Blocking reader
				let (d, read, buf) = tokio::task::spawn_blocking(move || {
					let mut buf = vec![0; 1024 * 64];
					match decrypted.read(&mut buf) {
						Ok(r) => Ok((decrypted, r, buf)),
						Err(e) => Err(e)
					}
				}).await??;
				decrypted = d;
				if read == 0 {
					break;
				}
				file.write_all(&buf[0..read]).await?;
				yield read;
			}
		}
	}
}

#[derive(Debug, Clone)]
pub enum AudioFormat {
	Ogg,
	Aac,
	Mp3,
	Flac,
}

impl AudioFormat {
	/// Get extension
	pub fn extension(&self) -> String {
		match self {
			AudioFormat::Ogg => "ogg",
			AudioFormat::Aac => "m4a",
			AudioFormat::Mp3 => "mp3",
			AudioFormat::Flac => "flac",
		}
		.to_string()
	}
}

impl From<AudioFileFormat> for AudioFormat {
	fn from(f: AudioFileFormat) -> Self {
		match f {
			AudioFileFormat::OGG_VORBIS_96 => Self::Ogg,
			AudioFileFormat::OGG_VORBIS_160 => Self::Ogg,
			AudioFileFormat::OGG_VORBIS_320 => Self::Ogg,
			AudioFileFormat::MP3_256 => Self::Mp3,
			AudioFileFormat::MP3_320 => Self::Mp3,
			AudioFileFormat::MP3_160 => Self::Mp3,
			AudioFileFormat::MP3_96 => Self::Mp3,
			AudioFileFormat::MP3_160_ENC => Self::Mp3,
			AudioFileFormat::AAC_24 => Self::Aac,
			AudioFileFormat::AAC_48 => Self::Aac,
			AudioFileFormat::FLAC_FLAC => Self::Flac,
		}
	}
}

impl Quality {
	/// Get librespot AudioFileFormat
	pub fn get_file_formats(&self) -> Vec<AudioFileFormat> {
		match self {
			Self::Q320 => vec![
				AudioFileFormat::OGG_VORBIS_320,
				AudioFileFormat::MP3_320,
				AudioFileFormat::FLAC_FLAC,
			],
			Self::Q256 => vec![AudioFileFormat::MP3_256],
			Self::Q160 => vec![
				AudioFileFormat::OGG_VORBIS_160,
				AudioFileFormat::MP3_160,
				AudioFileFormat::MP3_160_ENC,
				AudioFileFormat::AAC_24,
				AudioFileFormat::AAC_48,
			],
			Self::Q96 => vec![AudioFileFormat::OGG_VORBIS_96, AudioFileFormat::MP3_96],
		}
	}

	/// Fallback to lower quality
	pub fn fallback(&self) -> Option<Quality> {
		match self {
			Self::Q320 => Some(Quality::Q256),
			Self::Q256 => Some(Quality::Q160),
			Self::Q160 => Some(Quality::Q96),
			Self::Q96 => None,
		}
	}
}

#[derive(Debug, Clone)]
pub struct DownloadJob {
	pub id: i64,
	pub track_id: String,
}

#[derive(Debug, Clone)]
pub enum Message {
	// Send job to worker
	GetJob,
	// Update state of download
	UpdateState(i64, DownloadState),
	//add to download
	AddToQueue(Vec<Download>),
	// Get all downloads to UI
	GetDownloads,
}

#[derive(Debug, Clone)]
pub enum Response {
	Downloads(Vec<Download>),
}

#[derive(Debug, Clone)]
pub struct Download {
	pub id: i64,
	pub track_id: String,
	pub title: String,
	pub subtitle: String,
	pub state: DownloadState,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
	pub track_id: String,
	pub author: String,
	pub title: String,
}

impl From<aspotify::Track> for SearchResult {
	fn from(val: aspotify::Track) -> Self {
		SearchResult {
			track_id: val.id.unwrap(),
			author: val.artists[0].name.to_owned(),
			title: val.name,
		}
	}
}

impl From<aspotify::Track> for Download {
	fn from(val: aspotify::Track) -> Self {
		Download {
			id: 0,
			track_id: val.id.unwrap(),
			title: val.name,
			subtitle: val
				.artists
				.first()
				.map(|a| a.name.to_owned())
				.unwrap_or_default(),
			state: DownloadState::None,
		}
	}
}

impl From<aspotify::TrackSimplified> for Download {
	fn from(val: aspotify::TrackSimplified) -> Self {
		Download {
			id: 0,
			track_id: val.id.unwrap(),
			title: val.name,
			subtitle: val
				.artists
				.first()
				.map(|a| a.name.to_owned())
				.unwrap_or_default(),
			state: DownloadState::None,
		}
	}
}

impl From<Download> for DownloadJob {
	fn from(val: Download) -> Self {
		DownloadJob {
			id: val.id,
			track_id: val.track_id,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadState {
	None,
	Lock,
	Downloading(usize, usize, u64),
	Post,
	Done,
	Error(String),
}

/// Bitrate of music
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Copy)]
pub enum Quality {
	Q320,
	Q256,
	Q160,
	Q96,
}

impl ToString for Quality {
	fn to_string(&self) -> String {
		match self {
			Quality::Q320 => "320kbps",
			Quality::Q256 => "256kbps",
			Quality::Q160 => "160kbps",
			Quality::Q96 => "96kbps",
		}
		.to_string()
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloaderConfig {
	pub concurrent_downloads: usize,
	pub quality: Quality,
	pub path: String,
	pub tmp_path: String,
	pub filename_template: String,
	pub tmp_filename_template: String,
	pub timeout: u16,
	pub global_timeout: u16,
	pub id3v24: bool,
	pub convert_to_mp3: bool,
	pub separator: String,
	pub skip_existing: bool,
}

impl DownloaderConfig {
	// Create new instance
	pub fn new() -> DownloaderConfig {
		DownloaderConfig {
			concurrent_downloads: 4,
			quality: Quality::Q320,
			path: "downloads".to_string(),
			tmp_path: "downloads".to_string(),
			filename_template: "%artist% - %title%".to_string(),
			tmp_filename_template: "%artist% - %title%.tmp".to_string(),
			timeout: 300,
			global_timeout: 1800,
			id3v24: true,
			convert_to_mp3: false,
			separator: ", ".to_string(),
			skip_existing: true
		}
	}
}
