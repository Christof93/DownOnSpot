use aspotify::{
	Album, AlbumSimplified, Artist, Client, ClientCredentials, ItemType, Playlist, PlaylistItemType, Response, Track, TrackSimplified, TypeAlbum, TypeTrack
};
use librespot::core::authentication::Credentials;
use librespot::core::cache::Cache;
use librespot::core::config::SessionConfig;
use librespot::core::session::Session;
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;
use url::Url;

use crate::error::SpotifyError;

pub struct Spotify {
	// librespotify sessopm
	pub session: Session,
	pub spotify: Client,
}

impl Spotify {
	/// Create new instance
	pub async fn new(
		username: &str,
		password: &str,
		client_id: &str,
		client_secret: &str,
	) -> Result<Spotify, SpotifyError> {
		// librespot
		let credentials = Credentials::with_password(username, password);

		let session = Session::new(
			SessionConfig::default(),
			Some(Cache::new(Some(Path::new("credentials_cache")), None, None, None).unwrap()),
		);

		session.connect(credentials, true)
		.await?;

		//aspotify
		let credentials = ClientCredentials {
			id: client_id.to_string(),
			secret: client_secret.to_string(),
		};
		let spotify = Client::new(credentials);

		Ok(Spotify { session, spotify })
	}

	/// Parse URI or URL into URI
	pub fn parse_uri(uri: &str) -> Result<String, SpotifyError> {
		// Already URI
		if uri.starts_with("spotify:") {
			if uri.split(':').count() < 3 {
				return Err(SpotifyError::InvalidUri);
			}
			return Ok(uri.to_string());
		}

		// Parse URL
		let url = Url::parse(uri)?;
		// Spotify Web Player URL
		if url.host_str() == Some("open.spotify.com") {
			let path = url
				.path_segments()
				.ok_or_else(|| SpotifyError::Error("Missing URL path".into()))?
				.collect::<Vec<&str>>();
			if path.len() < 2 {
				return Err(SpotifyError::InvalidUri);
			}
			return Ok(format!("spotify:{}:{}", path[0], path[1]));
		}
		Err(SpotifyError::InvalidUri)
	}

	/// Fetch data for URI
	pub async fn resolve_uri(&self, uri: &str) -> Result<SpotifyItem, SpotifyError> {
		let parts = uri.split(':').skip(1).collect::<Vec<&str>>();
		let id = parts[1];
		match parts[0] {
			"track" => {
				// let track = self.spotify.tracks().get_track(id, None).await?;
				let album1 = AlbumSimplified {
					album_type: None,
					release_date: None,
					id: None,
					artists: Vec::new(),
					release_date_precision: None,
					available_markets: None,
					external_urls: HashMap::new(),
					images: Vec::new(),
					name: id.into(),
					restrictions: None,
					item_type: TypeAlbum,
				};
				let track = Track {
					album: album1,
					external_ids: HashMap::new(),
					popularity: 0,
					artists: Vec::new(),
					available_markets: Some(Vec::new()),
					disc_number: 0,
					duration: Duration::new(2,35),
					explicit:false,
					external_urls: HashMap::new(),
					id: Some(id.into()),
					is_playable: None,
					linked_from: None,
					restrictions: None,
					name: id.into(),
					track_number: 0,
					preview_url:None,
					item_type: TypeTrack,
					is_local:false,
				};
				// Ok(SpotifyItem::Track(track.data))
				Ok(SpotifyItem::Track(track))
			}
			"playlist" => {
				let playlist = self.spotify.playlists().get_playlist(id, None).await?;
				Ok(SpotifyItem::Playlist(playlist.data))
			}
			"album" => {
				let album = self.spotify.albums().get_album(id, None).await?;
				Ok(SpotifyItem::Album(album.data))
			}
			"artist" => {
				let artist = self.spotify.artists().get_artist(id).await?;
				Ok(SpotifyItem::Artist(artist.data))
			}
			// Unsupported / Unimplemented
			_ => Ok(SpotifyItem::Other(uri.to_string())),
		}
	}

	/// Get search results for query
	pub async fn search(&self, query: &str) -> Result<Vec<Track>, SpotifyError> {
		Ok(self
			.spotify
			.search()
			.search(query, [ItemType::Track], true, 50, 0, None)
			.await?
			.data
			.tracks
			.unwrap()
			.items)
	}

	/// Get all tracks from playlist
	pub async fn full_playlist(&self, id: &str) -> Result<Vec<Track>, SpotifyError> {
		let mut items = vec![];
		let mut offset = 0;
		loop {
			let page = self
				.spotify
				.playlists()
				.get_playlists_items(id, 100, offset, None)
				.await?;
			items.append(
				&mut page
					.data
					.items
					.iter()
					.filter_map(|i| -> Option<Track> {
						if let Some(PlaylistItemType::Track(t)) = &i.item {
							Some(t.to_owned())
						} else {
							None
						}
					})
					.collect(),
			);

			// End
			offset += page.data.items.len();
			if page.data.total == offset {
				return Ok(items);
			}
		}
	}

	/// Get all tracks from album
	pub async fn full_album(&self, id: &str) -> Result<Vec<TrackSimplified>, SpotifyError> {
		let mut items = vec![];
		let mut offset = 0;
		loop {
			let page = self
				.spotify
				.albums()
				.get_album_tracks(id, 50, offset, None)
				.await?;
			items.append(&mut page.data.items.to_vec());

			// End
			offset += page.data.items.len();
			if page.data.total == offset {
				return Ok(items);
			}
		}
	}

	/// Get all tracks from artist
	pub async fn full_artist(&self, id: &str) -> Result<Vec<TrackSimplified>, SpotifyError> {
		let mut items = vec![];
		let mut offset = 0;
		loop {
			let page = self
				.spotify
				.artists()
				.get_artist_albums(id, None, 50, offset, None)
				.await?;

			for album in &mut page.data.items.iter() {
				items.append(&mut self.full_album(&album.id).await?)
			}

			// End
			offset += page.data.items.len();
			if page.data.total == offset {
				return Ok(items);
			}
		}
	}
}

impl Clone for Spotify {
	fn clone(&self) -> Self {
		Self {
			session: self.session.clone(),
			spotify: Client::new(self.spotify.credentials.clone()),
		}
	}
}

/// Basic debug implementation so can be used in other structs
impl fmt::Debug for Spotify {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "<Spotify Instance>")
	}
}

#[derive(Debug, Clone)]
pub enum SpotifyItem {
	Track(Track),
	Album(Album),
	Playlist(Playlist),
	Artist(Artist),
	/// Unimplemented
	Other(String),
}
