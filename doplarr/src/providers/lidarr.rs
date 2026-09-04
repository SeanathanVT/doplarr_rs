use super::*;
use crate::{
    config::{BackendConfig, LidarrSearchMode},
    discord::MAX_DROPDOWN_OPTIONS,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::Datelike;
use lidarr_api::{
    apis::{
        Error as LidarrApiError,
        album_api::{api_v1_album_get, api_v1_album_monitor_put, api_v1_album_post},
        album_lookup_api::api_v1_album_lookup_get,
        artist_api::{api_v1_artist_id_get, api_v1_artist_id_put, api_v1_artist_post},
        artist_lookup_api::api_v1_artist_lookup_get,
        command_api::api_v1_command_post_custom,
        configuration::{ApiKey, Configuration},
        metadata_profile_api::api_v1_metadataprofile_get,
        quality_profile_api::api_v1_qualityprofile_get,
        root_folder_api::api_v1_rootfolder_get,
    },
    commands::AlbumSearchCommand,
    models::{
        AddAlbumOptions, AddArtistOptions, AlbumAddType, AlbumResource, AlbumsMonitoredResource,
        ArtistResource, MediaCover, MediaCoverTypes, MonitorTypes, NewItemMonitorTypes,
        RootFolderResource,
    },
};
use tracing::{debug, error, info, trace, warn};

/// Helper function to log detailed error information from Lidarr API responses
fn log_api_error<T: std::fmt::Debug>(err: &LidarrApiError<T>, context: &str) {
    match err {
        LidarrApiError::ResponseError(response) => {
            super::api_logging::log_api_error_details(response.status, &response.content, context);
            if let Some(ref entity) = response.entity {
                debug!("Parsed error entity: {:#?}", entity);
            }
        }
        LidarrApiError::Reqwest(e) => {
            error!("{} - Reqwest error: {}", context, e);
        }
        LidarrApiError::Serde(e) => {
            error!("{} - Serialization error: {}", context, e);
        }
        LidarrApiError::Io(e) => {
            error!("{} - IO error: {}", context, e);
        }
    }
}

/// Treat a 2xx response whose body fails to parse as success - by the time we're
/// reading the body, Lidarr has already applied the change
fn tolerate_response_parse_error<T, E>(
    result: std::result::Result<T, LidarrApiError<E>>,
    context: &str,
) -> Result<Option<T>>
where
    E: std::fmt::Debug + Send + Sync + 'static,
{
    match result {
        Ok(x) => Ok(Some(x)),
        Err(LidarrApiError::Serde(e)) => {
            warn!(
                "{} - succeeded, but the response body failed to parse: {}",
                context, e
            );
            Ok(None)
        }
        Err(e) => {
            log_api_error(&e, context);
            Err(e.into())
        }
    }
}

#[derive(Debug, Clone)]
pub struct Lidarr {
    config: Configuration,
    /// Monitor scopes offered when adding a new artist; a config pin collapses
    /// this to a single entry the requester never has to touch
    monitor: Vec<MonitorTypes>,
    /// Restricts search to one kind; `None` searches both
    search_mode: Option<LidarrSearchMode>,
    add_settings: AddSettings,
}

/// Where and how artists get added. All of it is resolved once at connect time
/// from the config and Lidarr's own defaults, because none of it is a decision a
/// requester should be asked to make.
#[derive(Debug, Clone)]
pub struct AddSettings {
    rootfolder_path: String,
    quality_profile_id: i32,
    metadata_profile_id: i32,
}

#[derive(Debug)]
// The final details needed to complete the request
pub struct SelectedDetails {
    /// How much of a new artist's discography to monitor
    pub monitor: Option<MonitorTypes>,
    /// Album ids picked from an existing artist's album picker
    pub album_ids: Vec<i32>,
    /// User chose "All Albums"
    pub all_albums: bool,
}

/// Pick the root folder to add into: the configured path, else whichever Lidarr
/// lists first, matching what Lidarr's own Add Artist screen preselects.
fn resolve_rootfolder(
    rootfolders: &[RootFolderResource],
    configured: Option<&str>,
) -> Result<RootFolderResource> {
    if let Some(path) = configured {
        return rootfolders
            .iter()
            .find(|rf| matches!(&rf.path, Some(Some(p)) if p == path))
            .cloned()
            .with_context(|| {
                let available = rootfolders
                    .iter()
                    .filter_map(|rf| rf.path.as_ref().and_then(|p| p.as_deref()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Root folder '{path}' not found. Available options: [{available}]")
            });
    }

    rootfolders
        .first()
        .cloned()
        .context("Lidarr has no root folders configured")
}

/// Resolve a quality or metadata profile to its id: the configured name, else the
/// default Lidarr has attached to the chosen root folder, else its first profile.
///
/// `label` names the profile in errors and logs, capitalized, e.g. "Quality profile".
fn resolve_profile(
    profiles: &[(i32, String)],
    configured: Option<&str>,
    rootfolder_default: Option<i32>,
    label: &str,
) -> Result<i32> {
    if let Some(name) = configured {
        return profiles
            .iter()
            .find(|(_, profile_name)| profile_name == name)
            .map(|(id, _)| *id)
            .with_context(|| {
                let available = profiles
                    .iter()
                    .map(|(_, profile_name)| profile_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{label} '{name}' not found. Available options: [{available}]")
            });
    }

    // A root folder can still point at a profile that has since been deleted
    if let Some(id) = rootfolder_default.filter(|id| profiles.iter().any(|(pid, _)| pid == id)) {
        debug!(
            profile_id = id,
            profile = label,
            "Using the root folder's default"
        );
        return Ok(id);
    }

    let id = profiles
        .first()
        .map(|(id, _)| *id)
        .with_context(|| format!("{label}s are not configured in Lidarr"))?;
    debug!(
        profile_id = id,
        profile = label,
        "Falling back to Lidarr's first"
    );
    Ok(id)
}

impl Lidarr {
    /// Builds the Lidarr connection and attempts to use it
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        base_path: String,
        key: String,
        quality_profile: Option<String>,
        metadata_profile: Option<String>,
        rootfolder: Option<String>,
        monitor_type: Option<MonitorTypes>,
        search_mode: Option<LidarrSearchMode>,
        client: reqwest::Client,
    ) -> Result<Self> {
        // Log connection before moving base_path
        info!("Connecting to Lidarr at {}", base_path);

        // Build the API config
        let config = Configuration {
            base_path,
            user_agent: None,
            client,
            basic_auth: None,
            oauth_access_token: None,
            bearer_access_token: None,
            api_key: Some(ApiKey { prefix: None, key }),
        };

        // Query everything the add settings resolve from (this will fail if we
        // can't connect to the server anyway)
        let rootfolders = api_v1_rootfolder_get(&config).await.inspect_err(|e| {
            log_api_error(e, "Failed to get root folders from Lidarr");
        })?;
        trace!("Retrieved {} root folders", rootfolders.len());

        let quality_profiles = api_v1_qualityprofile_get(&config).await.inspect_err(|e| {
            log_api_error(e, "Failed to get quality profiles from Lidarr");
        })?;
        trace!("Retrieved {} quality profiles", quality_profiles.len());

        let metadata_profiles = api_v1_metadataprofile_get(&config).await.inspect_err(|e| {
            log_api_error(e, "Failed to get metadata profiles from Lidarr");
        })?;
        trace!("Retrieved {} metadata profiles", metadata_profiles.len());

        let rootfolder = resolve_rootfolder(&rootfolders, rootfolder.as_deref())?;
        let rootfolder_path = rootfolder
            .path
            .clone()
            .flatten()
            .context("The selected Lidarr root folder has no path")?;

        let quality_pairs: Vec<(i32, String)> = quality_profiles
            .iter()
            .filter_map(|p| Some((p.id?, p.name.clone().flatten()?)))
            .collect();
        let metadata_pairs: Vec<(i32, String)> = metadata_profiles
            .iter()
            .filter_map(|p| Some((p.id?, p.name.clone().flatten()?)))
            .collect();

        let quality_profile_id = resolve_profile(
            &quality_pairs,
            quality_profile.as_deref(),
            rootfolder.default_quality_profile_id,
            "Quality profile",
        )?;
        let metadata_profile_id = resolve_profile(
            &metadata_pairs,
            metadata_profile.as_deref(),
            rootfolder.default_metadata_profile_id,
            "Metadata profile",
        )?;

        info!(
            rootfolder = %rootfolder_path,
            quality_profile_id,
            metadata_profile_id,
            "Resolved Lidarr add settings"
        );

        // Lidarr's own add-artist screen offers these, in this order. `Unknown` is
        // an internal placeholder that leaves the monitoring scope undefined.
        let monitor = if let Some(x) = monitor_type {
            if x == MonitorTypes::Unknown {
                bail!(
                    "Monitor type 'unknown' is not a valid scope. Available options: [all, future, missing, existing, latest, first, none]"
                );
            }
            vec![x]
        } else {
            vec![
                MonitorTypes::All,
                MonitorTypes::Future,
                MonitorTypes::Missing,
                MonitorTypes::Existing,
                MonitorTypes::Latest,
                MonitorTypes::First,
                MonitorTypes::None,
            ]
        };

        Ok(Self {
            config,
            monitor,
            search_mode,
            add_settings: AddSettings {
                rootfolder_path,
                quality_profile_id,
                metadata_profile_id,
            },
        })
    }

    pub async fn connect(backend: BackendConfig, client: reqwest::Client) -> Result<Self> {
        if let BackendConfig::Lidarr {
            url,
            api_key,
            quality_profile,
            metadata_profile,
            rootfolder,
            monitor_type,
            search_mode,
        } = backend
        {
            Self::new(
                url,
                api_key,
                quality_profile,
                metadata_profile,
                rootfolder,
                monitor_type,
                search_mode,
                client,
            )
            .await
        } else {
            bail!("Configured backend not for Lidarr");
        }
    }

    /// The monitoring scope to add a new artist with. Only shown for artists that
    /// aren't in the library yet, since it's an add-time decision Lidarr can't revisit.
    fn monitor_field(&self) -> RequestDetails {
        let options = self
            .monitor
            .iter()
            .map(|x| {
                let title = match x {
                    MonitorTypes::All => "All Albums",
                    MonitorTypes::Future => "Future Albums",
                    MonitorTypes::Missing => "Missing Albums",
                    MonitorTypes::Existing => "Existing Albums",
                    MonitorTypes::Latest => "Latest Album",
                    MonitorTypes::First => "First Album",
                    MonitorTypes::None => "None",
                    MonitorTypes::Unknown => "Unknown",
                };
                DropdownOption {
                    title: title.to_string(),
                    description: None,
                    id: Some(SelectableId::String(x.to_string())),
                }
            })
            .collect();

        RequestDetails {
            title: "Monitor".to_string(),
            options,
            metadata: Some(field_keys::MONITOR.to_string()),
            selected_indices: vec![],
            field_type: FieldType::Dropdown,
            always_show: false,
        }
    }

    /// Builds the multi-select album picker for an artist already in the library.
    ///
    /// Unlike the Sonarr season picker, this can't work off the search payload:
    /// Lidarr only knows an artist's albums once that artist has been added, so
    /// the list has to come from the library.
    async fn build_album_picker(&self, artist_id: i32) -> Result<RequestDetails> {
        let mut albums = api_v1_album_get(&self.config, Some(artist_id), None, None, None)
            .await
            .inspect_err(|e| {
                log_api_error(e, "Failed to list albums from Lidarr");
            })?;

        if albums.is_empty() {
            bail!(UserFacingError(
                "Lidarr doesn't list any albums for this artist.".into()
            ));
        }

        // Newest first. A long discography overruns Discord's option cap far more
        // easily than a season list does, and a recent release is the likelier ask.
        albums.sort_by_key(|album| std::cmp::Reverse(release_date(album)));

        // "All Albums" leads the list and is mutually exclusive with the rest
        let mut options = vec![DropdownOption {
            title: "All Albums".to_string(),
            description: Some("Includes future releases".to_string()),
            id: Some(SelectableId::Integer(ALL_ITEMS_ID)),
        }];

        // Reserve a slot for the "All Albums" entry against Discord's option cap
        let album_capacity = MAX_DROPDOWN_OPTIONS - options.len();
        if albums.len() > album_capacity {
            debug!(
                total = albums.len(),
                showing = album_capacity,
                "Truncating album list to fit Discord dropdown limit"
            );
        }

        options.extend(albums.iter().take(album_capacity).map(album_option));

        Ok(RequestDetails {
            title: "Albums".to_string(),
            options,
            metadata: Some(field_keys::ALBUM.to_string()),
            selected_indices: vec![],
            field_type: FieldType::MultiSelect,
            always_show: true,
        })
    }

    /// Keep grabbing an existing artist's future releases, which per-album
    /// monitoring flags can't express. Returns whether anything changed.
    async fn monitor_new_items(&self, artist_id: i32) -> Result<bool> {
        let mut artist = api_v1_artist_id_get(&self.config, artist_id)
            .await
            .inspect_err(|e| {
                log_api_error(e, "Failed to get existing artist from Lidarr");
            })?;

        if artist.monitor_new_items == Some(NewItemMonitorTypes::All) {
            return Ok(false);
        }

        artist.monitor_new_items = Some(NewItemMonitorTypes::All);
        artist.monitored = Some(true);

        tolerate_response_parse_error(
            api_v1_artist_id_put(
                &self.config,
                &artist_id.to_string(),
                Some(false),
                Some(artist),
            )
            .await,
            "Failed to monitor the artist's future releases",
        )?;
        debug!(artist_id, "Artist now monitors new items");

        Ok(true)
    }

    /// Monitor albums already in the library and search for them
    async fn monitor_albums(&self, album_ids: Vec<i32>) -> Result<()> {
        api_v1_album_monitor_put(
            &self.config,
            Some(AlbumsMonitoredResource {
                album_ids: Some(Some(album_ids.clone())),
                monitored: Some(true),
            }),
        )
        .await
        .inspect_err(|e| {
            log_api_error(e, "Failed to update album monitoring in Lidarr");
        })?;

        // One search command covers every newly monitored album
        let result = tolerate_response_parse_error(
            api_v1_command_post_custom(&self.config, &AlbumSearchCommand::new(album_ids)).await,
            "Failed to trigger album search",
        )?;
        info!(command_id = ?result.and_then(|r| r.id), "Album search queued");

        Ok(())
    }

    /// Add a new artist, or monitor more of one that's already in the library
    async fn request_artist(
        &self,
        selected: SelectedDetails,
        mut artist: ArtistResource,
    ) -> Result<()> {
        let name = artist.artist_name.clone().flatten().unwrap_or_default();

        // Existing artist: monitor the picked albums and search for them
        if let Some(id) = artist.id.filter(|id| *id > 0) {
            info!(artist_id = id, "Artist already exists in Lidarr");

            if selected.album_ids.is_empty() && !selected.all_albums {
                bail!(UserFacingError("No albums were selected.".into()));
            }

            let library_albums = api_v1_album_get(&self.config, Some(id), None, None, None)
                .await
                .inspect_err(|e| {
                    log_api_error(e, "Failed to list albums from Lidarr");
                })?;

            // Additive only: never unmonitor something the library already tracks
            let to_monitor: Vec<i32> = if selected.all_albums {
                library_albums
                    .iter()
                    .filter(|album| !album.monitored.unwrap_or(false))
                    .filter_map(|album| album.id)
                    .collect()
            } else {
                let already_monitored: Vec<i32> = library_albums
                    .iter()
                    .filter(|album| album.monitored.unwrap_or(false))
                    .filter_map(|album| album.id)
                    .collect();
                albums_to_monitor(&selected.album_ids, &already_monitored)
            };

            // "All Albums" also keeps future releases monitored, so it stays a
            // meaningful change even when everything released so far is already on
            let widened = selected.all_albums && self.monitor_new_items(id).await?;

            if to_monitor.is_empty() && !widened {
                bail!(UserFacingError(format!(
                    "Already monitored for {name}, nothing more to add."
                )));
            }
            debug!(
                ?to_monitor,
                all_albums = selected.all_albums,
                "Adding albums to monitoring"
            );

            if !to_monitor.is_empty() {
                self.monitor_albums(to_monitor).await?;
            }

            return Ok(());
        }

        info!("Artist is new, adding to Lidarr");

        let monitor = selected
            .monitor
            .context("No monitor type was selected for a new artist")?;
        let AddSettings {
            rootfolder_path,
            quality_profile_id,
            metadata_profile_id,
        } = &self.add_settings;

        debug!(
            rootfolder = %rootfolder_path,
            quality_profile_id,
            metadata_profile_id,
            monitor = %monitor,
            "Request details"
        );

        artist.root_folder_path = Some(Some(rootfolder_path.clone()));
        artist.quality_profile_id = Some(*quality_profile_id);
        artist.metadata_profile_id = Some(*metadata_profile_id);
        artist.monitored = Some(true);
        // Left unset this defaults to `all`, which would turn a narrow pick like
        // "Latest Album" into the whole discography as new albums appear
        artist.monitor_new_items = Some(match monitor {
            MonitorTypes::All | MonitorTypes::Future => NewItemMonitorTypes::All,
            _ => NewItemMonitorTypes::None,
        });
        artist.add_options = Some(Box::new(AddArtistOptions {
            monitor: Some(monitor),
            monitored: Some(true),
            search_for_missing_albums: Some(true),
            albums_to_monitor: None,
        }));

        info!(
            "Requesting artist: {} (mbid: {:?})",
            name, artist.foreign_artist_id
        );
        trace!("Full media object: {:#?}", artist);

        tolerate_response_parse_error(
            api_v1_artist_post(&self.config, Some(artist)).await,
            "Failed to add artist to Lidarr",
        )?;

        Ok(())
    }

    /// Add a single album, letting Lidarr create the artist if it needs to
    async fn request_album(&self, mut album: AlbumResource) -> Result<()> {
        // Lidarr already has a row for every album of an artist in the library,
        // and rejects adding one twice, so monitor it where it stands
        if let Some(id) = album.id.filter(|id| *id > 0) {
            info!(album_id = id, "Album already in Lidarr, monitoring it");
            return self.monitor_albums(vec![id]).await;
        }

        let AddSettings {
            rootfolder_path,
            quality_profile_id,
            metadata_profile_id,
        } = &self.add_settings;

        let mut artist = album
            .artist
            .clone()
            .context("Lidarr returned an album with no artist")?;
        let foreign_album_id = album
            .foreign_album_id
            .clone()
            .flatten()
            .context("Lidarr returned an album with no MusicBrainz id")?;

        artist.root_folder_path = Some(Some(rootfolder_path.clone()));
        artist.quality_profile_id = Some(*quality_profile_id);
        artist.metadata_profile_id = Some(*metadata_profile_id);
        artist.monitored = Some(true);
        // Setting the album's own monitoring isn't enough: the refresh that
        // follows the add re-adds the discography and decides each album itself,
        // so `albumsToMonitor` is what pins it to this one record. `Unknown` is
        // the only scope that defers to that list; `None` would also unmonitor
        // the artist.
        artist.monitor_new_items = Some(NewItemMonitorTypes::None);
        artist.add_options = Some(Box::new(AddArtistOptions {
            monitor: Some(MonitorTypes::Unknown),
            albums_to_monitor: Some(Some(vec![foreign_album_id])),
            monitored: Some(true),
            // Lidarr drops the album-specific search when the artist is
            // searching too, which would grab the whole discography instead
            search_for_missing_albums: Some(false),
        }));

        album.artist = Some(artist);
        album.monitored = Some(true);
        album.add_options = Some(Box::new(AddAlbumOptions {
            add_type: Some(AlbumAddType::Manual),
            search_for_new_album: Some(true),
        }));

        info!(
            "Requesting album: {} (mbid: {:?})",
            album.title.clone().flatten().unwrap_or_default(),
            album.foreign_album_id
        );
        debug!(
            rootfolder = %rootfolder_path,
            quality_profile_id,
            metadata_profile_id,
            "Request details"
        );
        trace!("Full media object: {:#?}", album);

        tolerate_response_parse_error(
            api_v1_album_post(&self.config, Some(album)).await,
            "Failed to add album to Lidarr",
        )?;

        Ok(())
    }
}

/// Helper function to get to and from stringified references
fn deserialize_from_string<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(&format!("\"{}\"", s))
        .with_context(|| format!("Failed to deserialize enum variant: {}", s))
}

/// The album's release date, flattened out of the doubly-optional generated field
fn release_date(album: &AlbumResource) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    album.release_date.flatten()
}

/// One entry in the album picker.
///
/// Already-monitored albums are listed and tagged rather than hidden, so the user
/// sees the whole discography; the request rejects those picks later.
fn album_option(album: &AlbumResource) -> DropdownOption {
    let mut tags = Vec::new();
    if let Some(album_type) = album.album_type.clone().flatten().filter(|t| !t.is_empty()) {
        tags.push(album_type);
    }
    if let Some(year) = release_date(album).map(|date| date.year()) {
        tags.push(year.to_string());
    }
    if album.monitored.unwrap_or(false) {
        tags.push("Already monitored".to_string());
    }

    DropdownOption {
        title: album.title.clone().flatten().unwrap_or_default(),
        description: (!tags.is_empty()).then(|| tags.join(" · ")),
        id: album.id.map(SelectableId::Integer),
    }
}

/// The artist an album belongs to, as carried by a lookup result
fn album_artist_name(album: &AlbumResource) -> Option<String> {
    album
        .artist
        .as_ref()
        .and_then(|artist| artist.artist_name.clone().flatten())
}

/// Discard anything Discord would reject as a thumbnail.
///
/// Lidarr rewrites `remoteUrl` to the local file it cached the cover into once
/// the media is in the library, so a value like `/data/MediaCover/2/poster.jpg`
/// comes back where a URL is expected. Sending that fails the whole component
/// with `URL_TYPE_INVALID_URL`, taking the interaction down with it, so anything
/// that isn't absolute is dropped in favour of no thumbnail at all.
fn usable_url(candidate: Option<String>) -> Option<String> {
    candidate.filter(|url| url.starts_with("https://") || url.starts_with("http://"))
}

/// The best usable image of the wanted kind, else any other usable one.
///
/// Lidarr returns several cover types per item and does not order them, so
/// without a preference the first entry is whatever the metadata source listed
/// first, typically a clearlogo rather than the artwork.
fn cover_url(images: Option<Vec<MediaCover>>, preferred: MediaCoverTypes) -> Option<String> {
    let images = images?;
    let usable = |cover: &MediaCover| usable_url(cover.remote_url.clone().flatten());

    images
        .iter()
        .find(|cover| cover.cover_type == Some(preferred))
        .and_then(usable)
        .or_else(|| images.iter().find_map(usable))
}

/// The best available cover art for an album
fn album_cover(album: &AlbumResource) -> Option<String> {
    usable_url(album.remote_cover.clone().flatten())
        .or_else(|| cover_url(album.images.clone().flatten(), MediaCoverTypes::Cover))
}

/// The best available image for an artist
fn artist_poster(artist: &ArtistResource) -> Option<String> {
    usable_url(artist.remote_poster.clone().flatten())
        .or_else(|| cover_url(artist.images.clone().flatten(), MediaCoverTypes::Poster))
}

/// Alternate between artist and album hits, keeping each list's own relevance
/// order. Concatenating instead would let Discord's option cap truncate one kind
/// away entirely on a broad search.
fn interleave(artists: Vec<LidarrMedia>, albums: Vec<LidarrMedia>) -> Vec<LidarrMedia> {
    let mut merged = Vec::with_capacity(artists.len() + albums.len());
    let mut artists = artists.into_iter();
    let mut albums = albums.into_iter();
    loop {
        match (artists.next(), albums.next()) {
            (None, None) => break,
            (artist, album) => merged.extend(artist.into_iter().chain(album)),
        }
    }
    merged
}

/// Returns the requested albums that aren't already monitored on the artist.
/// An empty result means every requested album was already monitored.
fn albums_to_monitor(requested: &[i32], already_monitored: &[i32]) -> Vec<i32> {
    requested
        .iter()
        .copied()
        .filter(|id| !already_monitored.contains(id))
        .collect()
}

/// Renders the picked albums for the one-line success summary. Album titles run
/// long, so more than one collapses to a count rather than a list.
fn format_albums(titles: &[String]) -> String {
    match titles {
        [] => String::new(),
        [title] => title.clone(),
        _ => format!("{} albums", titles.len()),
    }
}

/// The album titles the user picked, and whether they chose "All Albums"
fn selected_albums(details: &[RequestDetails]) -> (Vec<String>, bool) {
    let Some(detail) = details
        .iter()
        .find(|d| d.metadata.as_deref() == Some(field_keys::ALBUM))
    else {
        return (Vec::new(), false);
    };

    let mut titles = Vec::new();
    let mut all_albums = false;
    for option in detail.selected_options() {
        match &option.id {
            Some(SelectableId::Integer(ALL_ITEMS_ID)) => all_albums = true,
            _ => titles.push(option.title.clone()),
        }
    }
    (titles, all_albums)
}

/// A Lidarr search result.
///
/// Artist and album searches return different resources, but they share one media
/// type the way Seerr's movie and TV results do, so the backend downcasts once.
#[derive(Debug, Clone)]
pub enum LidarrMedia {
    Artist(ArtistResource),
    Album(AlbumResource),
}

mod field_keys {
    pub const MONITOR: &str = "lidarr:monitor";
    pub const ALBUM: &str = "lidarr:album";
}

impl TryFrom<Vec<RequestDetails>> for SelectedDetails {
    type Error = anyhow::Error;

    fn try_from(details: Vec<RequestDetails>) -> Result<Self> {
        let mut monitor = None;
        let mut album_ids = Vec::new();
        let mut all_albums = false;

        for detail in &details {
            // The album picker is multi-select; collect every chosen album.
            if detail.metadata.as_deref() == Some(field_keys::ALBUM) {
                for opt in detail.selected_options() {
                    match &opt.id {
                        Some(SelectableId::Integer(ALL_ITEMS_ID)) => all_albums = true,
                        Some(SelectableId::Integer(i)) => album_ids.push(*i),
                        other => bail!("Album must have an integer ID, got {other:?}"),
                    }
                }
                continue;
            }

            let Some(selection) = detail.selected_option() else {
                bail!("No option was selected for '{}'", detail.title);
            };

            match detail.metadata.as_deref() {
                Some(field_keys::MONITOR) => {
                    monitor = match &selection.id {
                        Some(SelectableId::String(s)) => Some(deserialize_from_string(s)?),
                        other => bail!("Monitor must have a string ID, got {other:?}"),
                    };
                }
                other => bail!("Unknown metadata key: {other:?}"),
            }
        }

        Ok(Self {
            monitor,
            album_ids,
            all_albums,
        })
    }
}

impl MediaItem for LidarrMedia {
    fn to_dropdown(&self) -> DropdownOption {
        match self {
            Self::Artist(artist) => {
                // Disambiguation is what MusicBrainz uses to tell same-named
                // artists apart, so it leads when present
                let mut tags = Vec::new();
                if let Some(detail) = artist
                    .disambiguation
                    .clone()
                    .flatten()
                    .filter(|d| !d.is_empty())
                    .or_else(|| artist.artist_type.clone().flatten())
                {
                    tags.push(detail);
                }
                // Artists deliberately don't early-stop, so this is the only
                // warning that picking this one changes a library entry rather
                // than adding something new. Lidarr's own search marks these too.
                if artist.id.is_some_and(|id| id > 0) {
                    tags.push("In library".to_string());
                }

                DropdownOption {
                    title: artist.artist_name.clone().flatten().unwrap_or_default(),
                    description: (!tags.is_empty()).then(|| tags.join(" · ")),
                    id: artist.id.map(SelectableId::Integer),
                }
            }
            Self::Album(album) => {
                // Leading with the release type doubles as the kind marker when
                // artists and albums share one list. The artist follows, since
                // album titles repeat across artists.
                let mut tags = vec![
                    album
                        .album_type
                        .clone()
                        .flatten()
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| "Album".to_string()),
                ];
                if let Some(artist) = album_artist_name(album) {
                    tags.push(artist);
                }
                if let Some(year) = release_date(album).map(|date| date.year()) {
                    tags.push(year.to_string());
                }

                DropdownOption {
                    title: album.title.clone().flatten().unwrap_or_default(),
                    description: Some(tags.join(" · ")),
                    id: album.id.map(SelectableId::Integer),
                }
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[async_trait]
impl MediaBackend for Lidarr {
    async fn search(&self, term: &str) -> Result<Vec<Box<dyn MediaItem>>> {
        let artists = self.search_mode != Some(LidarrSearchMode::Album);
        let albums = self.search_mode != Some(LidarrSearchMode::Artist);
        info!(artists, albums, "Searching Lidarr for: {}", term);

        // Both hit the same instance, so run them together and let either failure
        // sink the search rather than half-reporting
        let (artist_hits, album_hits) = tokio::join!(
            async {
                if !artists {
                    return Ok(Vec::new());
                }
                api_v1_artist_lookup_get(&self.config, Some(term))
                    .await
                    .inspect_err(|e| log_api_error(e, "Failed to search Lidarr for artists"))
            },
            async {
                if !albums {
                    return Ok(Vec::new());
                }
                api_v1_album_lookup_get(&self.config, Some(term))
                    .await
                    .inspect_err(|e| log_api_error(e, "Failed to search Lidarr for albums"))
            }
        );

        let artist_hits: Vec<LidarrMedia> =
            artist_hits?.into_iter().map(LidarrMedia::Artist).collect();
        let album_hits: Vec<LidarrMedia> =
            album_hits?.into_iter().map(LidarrMedia::Album).collect();
        debug!(
            artists = artist_hits.len(),
            albums = album_hits.len(),
            "Found results"
        );

        Ok(interleave(artist_hits, album_hits)
            .into_iter()
            .map(|m| Box::new(m) as Box<dyn MediaItem>)
            .collect())
    }

    fn to_dropdown_options(&self, results: &[Box<dyn MediaItem>]) -> Vec<DropdownOption> {
        // A filtered command says which kind it searches in its own name, so the
        // tag only earns its place when both kinds share one list
        if self.search_mode.is_some() {
            return results.iter().map(|x| x.to_dropdown()).collect();
        }

        results
            .iter()
            .filter_map(|r| r.as_any().downcast_ref::<LidarrMedia>())
            .map(|media| {
                let mut option = media.to_dropdown();
                // Albums already lead with their release type, so only artists
                // need saying out loud
                if let LidarrMedia::Artist(_) = media {
                    option.description = Some(match option.description {
                        Some(rest) => format!("Artist · {rest}"),
                        None => "Artist".to_string(),
                    });
                }
                option
            })
            .collect()
    }

    fn early_stop(&self, media: &dyn MediaItem) -> bool {
        let Some(media) = media.as_any().downcast_ref::<LidarrMedia>() else {
            error!("early_stop called with wrong media type for Lidarr backend");
            return false;
        };

        match media {
            // An artist already in the library still has albums worth requesting,
            // so the album picker decides; it raises a user-facing error when
            // there is genuinely nothing left.
            LidarrMedia::Artist(_) => false,
            // Adding an artist gives Lidarr a row for their whole discography, so
            // an id only means Lidarr knows the album. Monitoring is what tells us
            // somebody actually asked for it.
            LidarrMedia::Album(album) => {
                let monitored =
                    album.id.is_some_and(|id| id > 0) && album.monitored.unwrap_or(false);
                if monitored {
                    info!(album_id = ?album.id, "Album already monitored in Lidarr");
                }
                monitored
            }
        }
    }

    fn display_info(&self, media: &dyn MediaItem) -> MediaDisplayInfo {
        let Some(media) = media.as_any().downcast_ref::<LidarrMedia>() else {
            error!("display_info called with wrong media type for Lidarr backend");
            return MediaDisplayInfo {
                title: String::new(),
                subtitle: None,
                description: None,
                thumbnail_url: None,
            };
        };

        match media {
            LidarrMedia::Artist(artist) => MediaDisplayInfo {
                title: artist.artist_name.clone().flatten().unwrap_or_default(),
                subtitle: artist
                    .disambiguation
                    .clone()
                    .flatten()
                    .filter(|d| !d.is_empty())
                    .or_else(|| artist.artist_type.clone().flatten()),
                description: artist.overview.clone().flatten(),
                thumbnail_url: artist_poster(artist),
            },
            LidarrMedia::Album(album) => {
                let mut subtitle = Vec::new();
                if let Some(artist) = album_artist_name(album) {
                    subtitle.push(artist);
                }
                if let Some(year) = release_date(album).map(|date| date.year()) {
                    subtitle.push(year.to_string());
                }

                MediaDisplayInfo {
                    title: album.title.clone().flatten().unwrap_or_default(),
                    subtitle: (!subtitle.is_empty()).then(|| subtitle.join(" · ")),
                    description: album.overview.clone().flatten(),
                    thumbnail_url: album_cover(album),
                }
            }
        }
    }

    async fn additional_details(&self, media: &dyn MediaItem) -> Result<Vec<RequestDetails>> {
        let Some(media) = media.as_any().downcast_ref::<LidarrMedia>() else {
            error!("additional_details called with wrong media type for Lidarr backend");
            bail!("Invalid media type for Lidarr");
        };

        match media {
            LidarrMedia::Artist(artist) => match artist.id.filter(|id| *id > 0) {
                // Existing artist: where and how it was added is already settled,
                // so the only thing left is which albums to monitor
                Some(id) => {
                    debug!(
                        artist_id = id,
                        "Artist already exists, showing album picker"
                    );
                    Ok(vec![self.build_album_picker(id).await?])
                }
                None => Ok(vec![self.monitor_field()]),
            },
            // The album is the whole request and the add settings come from the
            // config, so there is nothing to ask
            LidarrMedia::Album(_) => Ok(Vec::new()),
        }
    }

    async fn request(
        &self,
        details: Vec<RequestDetails>,
        media: Box<dyn MediaItem>,
        _requester_discord_id: u64,
    ) -> Result<()> {
        let selected = SelectedDetails::try_from(details)?;

        // Downcast to concrete type
        let media = *media
            .into_any()
            .downcast::<LidarrMedia>()
            .map_err(|_| anyhow::anyhow!("Invalid media type for Lidarr"))?;

        match media {
            LidarrMedia::Artist(artist) => self.request_artist(selected, artist).await,
            LidarrMedia::Album(album) => self.request_album(album).await,
        }
    }

    fn success_message(&self, details: &[RequestDetails], media: &dyn MediaItem) -> SuccessMessage {
        let Some(media) = media.as_any().downcast_ref::<LidarrMedia>() else {
            error!("success_message called with wrong media type for Lidarr backend");
            return SuccessMessage {
                summary: "Request submitted".into(),
                description: "Will be downloaded when available.".into(),
                thumbnail_url: None,
                embed_data: None,
            };
        };

        match media {
            LidarrMedia::Artist(artist) => {
                let name = artist.artist_name.clone().flatten().unwrap_or_default();
                let overview = artist.overview.clone().flatten().unwrap_or_default();
                let genres: Vec<String> = artist.genres.clone().flatten().unwrap_or_default();
                let poster = artist_poster(artist);

                // Name the albums when the user picked from an existing artist
                let (titles, all_albums) = selected_albums(details);
                let detail_text = if all_albums {
                    " (All Albums)".to_string()
                } else {
                    match format_albums(&titles) {
                        s if s.is_empty() => String::new(),
                        s => format!(" ({s})"),
                    }
                };

                let external_url = artist
                    .foreign_artist_id
                    .clone()
                    .flatten()
                    .map(|id| format!("https://musicbrainz.org/artist/{id}"));

                let embed_data = external_url.map(|external_url| EmbedData {
                    title: format!("{name}{detail_text}"),
                    media_type: "Artist",
                    overview: truncate_for_embed(&overview),
                    poster_url: poster.clone().unwrap_or_default(),
                    genres,
                    runtime_minutes: None,
                    studio_or_network: None,
                    director: None,
                    external_url,
                });

                SuccessMessage {
                    summary: format!("{name}{detail_text}"),
                    description: "Will be downloaded when available.".to_string(),
                    thumbnail_url: poster,
                    embed_data,
                }
            }
            LidarrMedia::Album(album) => {
                let title = album.title.clone().flatten().unwrap_or_default();
                let artist = album_artist_name(album);
                let overview = album.overview.clone().flatten().unwrap_or_default();
                let genres: Vec<String> = album.genres.clone().flatten().unwrap_or_default();
                let cover = album_cover(album);

                let summary = match &artist {
                    Some(artist) => format!("{artist} - {title}"),
                    None => title.clone(),
                };

                // Lidarr reports duration in milliseconds, and leaves it at zero
                // for an album it hasn't added yet, so only show a known runtime
                let runtime_minutes = album
                    .duration
                    .filter(|duration| *duration > 0)
                    .map(|duration| (duration as u32) / 60_000);

                let external_url = album
                    .foreign_album_id
                    .clone()
                    .flatten()
                    .map(|id| format!("https://musicbrainz.org/release-group/{id}"));

                let embed_data = external_url.map(|external_url| EmbedData {
                    title: summary.clone(),
                    media_type: "Album",
                    overview: truncate_for_embed(&overview),
                    poster_url: cover.clone().unwrap_or_default(),
                    genres,
                    runtime_minutes,
                    studio_or_network: artist,
                    director: None,
                    external_url,
                });

                SuccessMessage {
                    summary,
                    description: "Will be downloaded when available.".to_string(),
                    thumbnail_url: cover,
                    embed_data,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A multi-select album picker over the given album ids, with the options at
    /// `selected` indices chosen.
    fn album_field(albums: &[(i32, &str)], selected: &[usize]) -> RequestDetails {
        RequestDetails {
            title: "Albums".into(),
            options: albums
                .iter()
                .map(|(id, title)| DropdownOption {
                    title: title.to_string(),
                    description: None,
                    id: Some(SelectableId::Integer(*id)),
                })
                .collect(),
            selected_indices: selected.to_vec(),
            metadata: Some(field_keys::ALBUM.to_string()),
            field_type: FieldType::MultiSelect,
            always_show: true,
        }
    }

    fn monitor_field(id: &str, selected: bool) -> RequestDetails {
        RequestDetails {
            title: "Monitor".into(),
            options: vec![DropdownOption {
                title: "All Albums".into(),
                description: None,
                id: Some(SelectableId::String(id.into())),
            }],
            selected_indices: if selected { vec![0] } else { vec![] },
            metadata: Some(field_keys::MONITOR.to_string()),
            field_type: FieldType::Dropdown,
            always_show: false,
        }
    }

    #[test]
    fn try_from_reads_the_monitor_scope() {
        let selected = SelectedDetails::try_from(vec![monitor_field("all", true)]).unwrap();
        assert_eq!(selected.monitor, Some(MonitorTypes::All));
        assert!(selected.album_ids.is_empty());
        assert!(!selected.all_albums);
    }

    #[test]
    fn try_from_auto_selects_a_pinned_monitor_scope() {
        // A config pin collapses the field to one option the user never touches
        let selected = SelectedDetails::try_from(vec![monitor_field("latest", false)]).unwrap();
        assert_eq!(selected.monitor, Some(MonitorTypes::Latest));
    }

    #[test]
    fn try_from_accepts_no_details_at_all() {
        // Album mode asks nothing: every add setting comes from the config
        let selected = SelectedDetails::try_from(Vec::new()).unwrap();
        assert!(selected.monitor.is_none());
        assert!(selected.album_ids.is_empty());
    }

    #[test]
    fn try_from_errors_on_an_unselected_multi_option_field() {
        let mut detail = monitor_field("all", false);
        detail.options.push(DropdownOption {
            title: "None".into(),
            description: None,
            id: Some(SelectableId::String("none".into())),
        });
        assert!(SelectedDetails::try_from(vec![detail]).is_err());
    }

    #[test]
    fn try_from_collects_multiple_albums() {
        let details = vec![album_field(
            &[(7, "Geogaddi"), (8, "Tomorrow's Harvest")],
            &[0, 1],
        )];
        let selected = SelectedDetails::try_from(details).unwrap();
        assert_eq!(selected.album_ids, vec![7, 8]);
        assert!(!selected.all_albums);
    }

    #[test]
    fn try_from_all_albums_sentinel() {
        let details = vec![album_field(
            &[(ALL_ITEMS_ID, "All Albums"), (7, "Geogaddi")],
            &[0],
        )];
        let selected = SelectedDetails::try_from(details).unwrap();
        assert!(selected.all_albums);
        assert!(selected.album_ids.is_empty());
    }

    #[test]
    fn albums_to_monitor_skips_already_monitored() {
        assert_eq!(albums_to_monitor(&[1, 2, 3], &[2]), vec![1, 3]);
        assert!(albums_to_monitor(&[1, 2], &[1, 2, 3]).is_empty());
    }

    #[test]
    fn format_albums_names_one_and_counts_more() {
        // Album titles are long, so only a single pick is named outright
        assert_eq!(format_albums(&[]), "");
        assert_eq!(format_albums(&["Geogaddi".to_string()]), "Geogaddi");
        assert_eq!(
            format_albums(&["Geogaddi".to_string(), "Amnesiac".to_string()]),
            "2 albums"
        );
    }

    #[test]
    fn selected_albums_separates_the_all_sentinel_from_titles() {
        let details = vec![album_field(
            &[
                (ALL_ITEMS_ID, "All Albums"),
                (7, "Geogaddi"),
                (8, "Amnesiac"),
            ],
            &[0, 1],
        )];
        let (titles, all_albums) = selected_albums(&details);
        assert!(all_albums);
        assert_eq!(titles, vec!["Geogaddi"]);
    }

    /// An album as the library reports it, for picker-label tests.
    fn album(title: &str, album_type: &str, year: i32, monitored: bool) -> AlbumResource {
        AlbumResource {
            id: Some(1),
            title: Some(Some(title.to_string())),
            album_type: Some(Some(album_type.to_string())),
            monitored: Some(monitored),
            release_date: Some(Some(
                chrono::DateTime::parse_from_rfc3339(&format!("{year}-01-01T00:00:00Z")).unwrap(),
            )),
            ..AlbumResource::new()
        }
    }

    /// A backend with no live connection, for `early_stop` decisions.
    fn test_lidarr(search_mode: Option<LidarrSearchMode>) -> Lidarr {
        // Skip system CA loading so the test works in sandboxed environments (e.g. Nix).
        let client = reqwest::ClientBuilder::new()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        Lidarr {
            config: Configuration {
                base_path: "http://localhost:8686".to_owned(),
                user_agent: None,
                client,
                basic_auth: None,
                oauth_access_token: None,
                bearer_access_token: None,
                api_key: None,
            },
            monitor: vec![],
            search_mode,
            add_settings: AddSettings {
                rootfolder_path: "/music".to_owned(),
                quality_profile_id: 1,
                metadata_profile_id: 1,
            },
        }
    }

    #[test]
    fn early_stop_lets_an_unmonitored_album_through() {
        // Adding an artist gives Lidarr a row for their whole discography, so an
        // id alone must not read as "already requested"
        let lidarr = test_lidarr(Some(LidarrSearchMode::Album));
        let media = LidarrMedia::Album(AlbumResource {
            id: Some(42),
            monitored: Some(false),
            ..AlbumResource::new()
        });
        assert!(!lidarr.early_stop(&media));
    }

    #[test]
    fn early_stop_halts_on_a_monitored_album() {
        let lidarr = test_lidarr(Some(LidarrSearchMode::Album));
        let media = LidarrMedia::Album(AlbumResource {
            id: Some(42),
            monitored: Some(true),
            ..AlbumResource::new()
        });
        assert!(lidarr.early_stop(&media));
    }

    #[test]
    fn early_stop_never_halts_on_an_artist() {
        // An artist in the library still has albums worth requesting
        let lidarr = test_lidarr(Some(LidarrSearchMode::Artist));
        let media = LidarrMedia::Artist(ArtistResource {
            id: Some(42),
            monitored: Some(true),
            ..ArtistResource::new()
        });
        assert!(!lidarr.early_stop(&media));
    }

    #[test]
    fn picker_tags_an_album_with_its_type_and_year() {
        let option = album_option(&album("Geogaddi", "Album", 2002, false));
        assert_eq!(option.title, "Geogaddi");
        assert_eq!(option.description.as_deref(), Some("Album · 2002"));
    }

    #[test]
    fn picker_marks_monitored_albums() {
        // Monitored albums stay listed so the user sees the whole discography
        let option = album_option(&album("Geogaddi", "Album", 2002, true));
        assert_eq!(
            option.description.as_deref(),
            Some("Album · 2002 · Already monitored")
        );
    }

    #[test]
    fn picker_omits_the_subtitle_when_lidarr_knows_nothing() {
        let option = album_option(&AlbumResource {
            title: Some(Some("Untitled".to_string())),
            ..AlbumResource::new()
        });
        assert_eq!(option.title, "Untitled");
        assert!(option.description.is_none());
    }

    fn artist(name: &str) -> LidarrMedia {
        LidarrMedia::Artist(ArtistResource {
            artist_name: Some(Some(name.to_string())),
            ..ArtistResource::new()
        })
    }

    fn album_item(title: &str) -> LidarrMedia {
        LidarrMedia::Album(AlbumResource {
            title: Some(Some(title.to_string())),
            album_type: Some(Some("Album".to_string())),
            ..AlbumResource::new()
        })
    }

    fn titles(media: &[LidarrMedia]) -> Vec<String> {
        media.iter().map(|m| m.to_dropdown().title).collect()
    }

    fn cover(kind: MediaCoverTypes, remote_url: &str) -> MediaCover {
        MediaCover {
            cover_type: Some(kind),
            remote_url: Some(Some(remote_url.to_string())),
            ..MediaCover::new()
        }
    }

    fn artist_with_images(images: Vec<MediaCover>) -> ArtistResource {
        ArtistResource {
            images: Some(Some(images)),
            ..ArtistResource::new()
        }
    }

    #[test]
    fn a_cached_cover_path_is_not_offered_to_discord() {
        // Lidarr hands back the local file once the media is in the library, and
        // Discord rejects the whole component if that reaches it
        let artist = artist_with_images(vec![cover(
            MediaCoverTypes::Poster,
            "/opt/lidarr-data/MediaCover/2/poster.jpg",
        )]);
        assert_eq!(artist_poster(&artist), None);
    }

    #[test]
    fn the_poster_wins_over_whatever_lidarr_lists_first() {
        let artist = artist_with_images(vec![
            cover(
                MediaCoverTypes::Clearlogo,
                "https://example.invalid/logo.png",
            ),
            cover(
                MediaCoverTypes::Poster,
                "https://example.invalid/poster.jpg",
            ),
        ]);
        assert_eq!(
            artist_poster(&artist).as_deref(),
            Some("https://example.invalid/poster.jpg")
        );
    }

    #[test]
    fn any_usable_cover_beats_no_thumbnail() {
        // No poster, but a real URL is still better than nothing
        let artist = artist_with_images(vec![cover(
            MediaCoverTypes::Clearlogo,
            "https://example.invalid/logo.png",
        )]);
        assert_eq!(
            artist_poster(&artist).as_deref(),
            Some("https://example.invalid/logo.png")
        );
    }

    #[test]
    fn a_remote_poster_is_still_preferred_when_it_is_a_url() {
        let artist = ArtistResource {
            remote_poster: Some(Some("https://example.invalid/remote.jpg".to_string())),
            ..artist_with_images(vec![cover(
                MediaCoverTypes::Poster,
                "https://example.invalid/poster.jpg",
            )])
        };
        assert_eq!(
            artist_poster(&artist).as_deref(),
            Some("https://example.invalid/remote.jpg")
        );
    }

    #[test]
    fn an_artist_already_in_the_library_says_so() {
        let media = LidarrMedia::Artist(ArtistResource {
            id: Some(7),
            artist_type: Some(Some("Group".to_string())),
            ..ArtistResource::new()
        });
        assert_eq!(
            media.to_dropdown().description.as_deref(),
            Some("Group · In library")
        );
    }

    #[test]
    fn an_artist_lidarr_does_not_have_is_unmarked() {
        // A lookup miss reports id 0 rather than omitting it, so both must read
        // as "not in the library"
        for id in [None, Some(0)] {
            let media = LidarrMedia::Artist(ArtistResource {
                id,
                artist_type: Some(Some("Group".to_string())),
                ..ArtistResource::new()
            });
            assert_eq!(media.to_dropdown().description.as_deref(), Some("Group"));
        }
    }

    #[test]
    fn interleave_alternates_between_the_two_lists() {
        // Truncation to Discord's cap must not be able to drop one kind entirely
        let merged = interleave(
            vec![artist("A1"), artist("A2")],
            vec![album_item("B1"), album_item("B2")],
        );
        assert_eq!(titles(&merged), ["A1", "B1", "A2", "B2"]);
    }

    #[test]
    fn interleave_appends_the_longer_lists_tail() {
        let merged = interleave(
            vec![artist("A1")],
            vec![album_item("B1"), album_item("B2"), album_item("B3")],
        );
        assert_eq!(titles(&merged), ["A1", "B1", "B2", "B3"]);
    }

    #[test]
    fn interleave_handles_an_empty_side() {
        assert_eq!(titles(&interleave(vec![], vec![album_item("B1")])), ["B1"]);
        assert_eq!(titles(&interleave(vec![artist("A1")], vec![])), ["A1"]);
        assert!(interleave(vec![], vec![]).is_empty());
    }

    #[test]
    fn combined_search_tags_each_result_with_its_kind() {
        let lidarr = test_lidarr(None);
        let results: Vec<Box<dyn MediaItem>> = vec![
            Box::new(artist("Bad Religion")),
            Box::new(album_item("Hypercaffium Spazzinate")),
        ];
        let options = lidarr.to_dropdown_options(&results);
        assert!(
            options[0]
                .description
                .as_deref()
                .unwrap()
                .starts_with("Artist"),
            "{:?}",
            options[0].description
        );
        // Albums announce themselves through their release type, not a second tag
        assert_eq!(options[1].description.as_deref(), Some("Album"));
    }

    #[test]
    fn a_filtered_search_leaves_the_subtitle_alone() {
        // The command name already says which kind it searches
        let lidarr = test_lidarr(Some(LidarrSearchMode::Artist));
        let results: Vec<Box<dyn MediaItem>> = vec![Box::new(artist("Bad Religion"))];
        let tagged = lidarr.to_dropdown_options(&results);
        assert_eq!(
            tagged[0].description,
            artist("Bad Religion").to_dropdown().description
        );
    }

    fn rootfolder(path: &str, quality: i32, metadata: i32) -> RootFolderResource {
        RootFolderResource {
            path: Some(Some(path.to_string())),
            default_quality_profile_id: Some(quality),
            default_metadata_profile_id: Some(metadata),
            ..RootFolderResource::new()
        }
    }

    fn profiles() -> Vec<(i32, String)> {
        vec![(1, "Standard".to_string()), (2, "Lossless".to_string())]
    }

    #[test]
    fn rootfolder_prefers_the_configured_path() {
        let folders = vec![rootfolder("/music", 1, 1), rootfolder("/music2", 2, 2)];
        let resolved = resolve_rootfolder(&folders, Some("/music2")).unwrap();
        assert_eq!(resolved.path.flatten().as_deref(), Some("/music2"));
    }

    #[test]
    fn rootfolder_falls_back_to_the_first_rather_than_asking() {
        let folders = vec![rootfolder("/music", 1, 1), rootfolder("/music2", 2, 2)];
        let resolved = resolve_rootfolder(&folders, None).unwrap();
        assert_eq!(resolved.path.flatten().as_deref(), Some("/music"));
    }

    #[test]
    fn rootfolder_errors_with_the_available_options() {
        let folders = vec![rootfolder("/music", 1, 1)];
        let err = resolve_rootfolder(&folders, Some("/nope"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("/nope") && err.contains("/music"), "{err}");
    }

    #[test]
    fn rootfolder_errors_when_lidarr_has_none() {
        assert!(resolve_rootfolder(&[], None).is_err());
    }

    #[test]
    fn profile_prefers_the_config_pin() {
        let id =
            resolve_profile(&profiles(), Some("Lossless"), Some(1), "Quality profile").unwrap();
        assert_eq!(id, 2);
    }

    #[test]
    fn profile_falls_back_to_the_root_folder_default() {
        let id = resolve_profile(&profiles(), None, Some(2), "Quality profile").unwrap();
        assert_eq!(id, 2);
    }

    #[test]
    fn profile_ignores_a_root_folder_default_that_no_longer_exists() {
        // A deleted profile leaves a dangling id behind on the root folder
        let id = resolve_profile(&profiles(), None, Some(99), "Metadata profile").unwrap();
        assert_eq!(id, 1);
    }

    #[test]
    fn profile_errors_naming_the_available_options() {
        let err = resolve_profile(&profiles(), Some("Nope"), None, "Quality profile")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Quality profile 'Nope'") && err.contains("Standard, Lossless"),
            "{err}"
        );
    }

    #[test]
    fn profile_errors_when_lidarr_has_none() {
        assert!(resolve_profile(&[], None, None, "Metadata profile").is_err());
    }
}
