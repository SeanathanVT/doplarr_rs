/// Command payloads for Lidarr API
/// Reference: https://github.com/Lidarr/Lidarr/tree/develop/src/NzbDrone.Core/IndexerSearch
use serde::Serialize;

/// Minimal ArtistSearch command payload
/// Reference: https://github.com/Lidarr/Lidarr/blob/develop/src/NzbDrone.Core/IndexerSearch/ArtistSearchCommand.cs
#[derive(Debug, Clone, Serialize)]
pub struct ArtistSearchCommand {
    name: String,
    #[serde(rename = "artistId")]
    pub artist_id: i32,
}

impl ArtistSearchCommand {
    pub fn new(artist_id: i32) -> Self {
        Self {
            name: "ArtistSearch".to_string(),
            artist_id,
        }
    }
}

/// Minimal AlbumSearch command payload
/// Reference: https://github.com/Lidarr/Lidarr/blob/develop/src/NzbDrone.Core/IndexerSearch/AlbumSearchCommand.cs
#[derive(Debug, Clone, Serialize)]
pub struct AlbumSearchCommand {
    name: String,
    #[serde(rename = "albumIds")]
    pub album_ids: Vec<i32>,
}

impl AlbumSearchCommand {
    pub fn new(album_ids: Vec<i32>) -> Self {
        Self {
            name: "AlbumSearch".to_string(),
            album_ids,
        }
    }
}
