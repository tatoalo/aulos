//! The opaque [`MediaEntry::state`](aulos_provider::entry::MediaEntry::state) blob this provider
//! round-trips (DESIGN §10.3), and the **legacy flat** `.info.json` shape written next to the
//! finished file (DESIGN §10.5).
//!
//! These are two different surfaces with two different consumers, and conflating them is the
//! mistake this module exists to prevent:
//!
//! | Surface | Shape | Read by |
//! |---|---|---|
//! | `entry_json` in the DB, [`ScState`] | v2: `base_url`, `title_id`, `episode_id`, `needs_m3u8_extraction`, `legacy` | [`crate::jit`], the NFO hook |
//! | `<title>.info.json` on disk | legacy flat: `_sc_base_url`, `_sc_needs_m3u8_extraction`, keys at the top level | users' own `Exec` postprocessors, the legacy `jellyfin_nfo_generator.py` CLI |
//!
//! `state.legacy` carries every key an imported legacy row had that v2 has no field for (`plot`,
//! `upload_date`, `uploader`, `tags`, …) so the NFO hook keeps producing the same XML for a
//! pre-cutover row, and [`ScState::to_legacy_info_json`] spreads it back at the top level.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{EXTRACTOR, EXTRACTOR_KEY};

/// The provider-native state persisted with an SC entry (DESIGN §10.3).
///
/// Every field the just-in-time extractor and the NFO hook need, and nothing the engine looks
/// inside.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ScState {
    /// `{scheme}://{host}` of the site this entry came from. Legacy's `_sc_base_url`.
    pub base_url: String,
    /// The numeric title id, which legacy only ever had embedded in `id`.
    #[serde(default)]
    pub title_id: Option<u64>,
    /// The numeric episode id, for a TV episode.
    #[serde(default)]
    pub episode_id: Option<u64>,
    /// Always `true` for this provider: the stream URL is re-extracted at download time because
    /// vixcloud tokens expire in minutes. Legacy's `_sc_needs_m3u8_extraction`.
    pub needs_m3u8_extraction: bool,
    /// 1-based season number, for a TV episode.
    #[serde(default)]
    pub season_number: Option<u32>,
    /// 1-based episode number, for a TV episode.
    #[serde(default)]
    pub episode_number: Option<u32>,
    /// The episode name. Empty when the site has none, exactly as legacy stored it.
    #[serde(default)]
    pub episode: String,
    /// The series name, for a TV episode. `null` for a movie, as legacy had it.
    #[serde(default)]
    pub series: Option<String>,
    /// The container extension, always `"mp4"`.
    pub ext: String,
    /// `"streamingcommunity"`.
    pub extractor: String,
    /// `"StreamingCommunity"`.
    pub extractor_key: String,
    /// Keys carried over verbatim from an imported legacy entry (DESIGN §7.6.3a).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub legacy: Map<String, Value>,
}

impl ScState {
    /// A movie's state.
    #[must_use]
    pub fn movie(base_url: &str, title_id: Option<u64>) -> Self {
        Self {
            base_url: base_url.to_owned(),
            title_id,
            episode_id: None,
            needs_m3u8_extraction: true,
            season_number: None,
            episode_number: None,
            episode: String::new(),
            series: None,
            ext: "mp4".to_owned(),
            extractor: EXTRACTOR.to_owned(),
            extractor_key: EXTRACTOR_KEY.to_owned(),
            legacy: Map::new(),
        }
    }

    /// An episode's state.
    #[must_use]
    pub fn episode(
        base_url: &str,
        title_id: Option<u64>,
        episode_id: Option<u64>,
        season_number: u32,
        episode_number: u32,
        episode_name: &str,
        series: &str,
    ) -> Self {
        Self {
            episode_id,
            season_number: Some(season_number),
            episode_number: Some(episode_number),
            episode: episode_name.to_owned(),
            series: Some(series.to_owned()),
            ..Self::movie(base_url, title_id)
        }
    }

    /// The state as the opaque JSON the engine stores.
    ///
    /// Infallible in practice — every field is a string, a number, a bool or a JSON object — and a
    /// serialisation failure degrades to `null` rather than failing a resolution, because a
    /// missing state blob only costs the just-in-time extractor its shortcut: it re-derives the
    /// ids from the watch URL (DESIGN §7.6.3a).
    #[must_use]
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Reads a state blob back, tolerating an absent or unexpected shape.
    #[must_use]
    pub fn from_json(v: &Value) -> Option<Self> {
        serde_json::from_value(v.clone()).ok()
    }

    /// The `<title>.info.json` sidecar, in the **legacy flat shape** (DESIGN §10.3 note 2).
    ///
    /// Key order matches legacy's dict literal (`streamingcommunity.py:170-187`) so a diff against
    /// a file captured from the Python server is empty. `state.legacy` is spread first so a real v2
    /// key always wins over a stale imported one.
    #[must_use]
    pub fn to_legacy_info_json(&self, media_id: &str, title: &str, url: &str) -> Value {
        let mut map = Map::new();
        for (k, v) in &self.legacy {
            map.insert(k.clone(), v.clone());
        }
        map.insert("id".to_owned(), Value::String(media_id.to_owned()));
        map.insert("title".to_owned(), Value::String(title.to_owned()));
        map.insert("url".to_owned(), Value::String(url.to_owned()));
        map.insert("webpage_url".to_owned(), Value::String(url.to_owned()));
        map.insert("ext".to_owned(), Value::String(self.ext.clone()));
        map.insert("_type".to_owned(), Value::String("video".to_owned()));
        map.insert(
            "extractor".to_owned(),
            Value::String(self.extractor.clone()),
        );
        map.insert(
            "extractor_key".to_owned(),
            Value::String(self.extractor_key.clone()),
        );
        map.insert(
            "season_number".to_owned(),
            number_or_null(self.season_number),
        );
        map.insert(
            "episode_number".to_owned(),
            number_or_null(self.episode_number),
        );
        map.insert("episode".to_owned(), Value::String(self.episode.clone()));
        map.insert(
            "series".to_owned(),
            self.series.clone().map_or(Value::Null, Value::String),
        );
        map.insert(
            "_sc_needs_m3u8_extraction".to_owned(),
            Value::Bool(self.needs_m3u8_extraction),
        );
        map.insert(
            "_sc_base_url".to_owned(),
            Value::String(self.base_url.clone()),
        );
        Value::Object(map)
    }
}

fn number_or_null(n: Option<u32>) -> Value {
    n.map_or(Value::Null, Value::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_movie_state_round_trips_and_omits_an_empty_legacy_map() {
        let s = ScState::movie("https://sc.test", Some(1234));
        let json = s.to_json();
        assert_eq!(json["base_url"], "https://sc.test");
        assert_eq!(json["title_id"], 1234);
        assert_eq!(json["needs_m3u8_extraction"], true);
        assert_eq!(json["ext"], "mp4");
        assert_eq!(json["extractor"], "streamingcommunity");
        assert_eq!(json["extractor_key"], "StreamingCommunity");
        assert!(
            json.get("legacy").is_none(),
            "an empty legacy map is elided"
        );
        assert_eq!(ScState::from_json(&json), Some(s));
    }

    #[test]
    fn an_episode_state_carries_the_series_fields() {
        let s = ScState::episode(
            "https://sc.test",
            Some(9),
            Some(77),
            2,
            3,
            "Il segreto",
            "Una serie",
        );
        assert_eq!(s.season_number, Some(2));
        assert_eq!(s.episode_number, Some(3));
        assert_eq!(s.episode, "Il segreto");
        assert_eq!(s.series.as_deref(), Some("Una serie"));
        assert_eq!(s.episode_id, Some(77));
        assert!(s.needs_m3u8_extraction);
    }

    #[test]
    fn the_sidecar_is_the_legacy_flat_shape() {
        let s = ScState::episode(
            "https://sc.test",
            Some(9),
            Some(77),
            1,
            2,
            "Pilota",
            "Serie",
        );
        let j = s.to_legacy_info_json(
            "sc_9_77",
            "Serie S01E02 - Pilota",
            "https://sc.test/it/watch/9?e=77",
        );
        // The legacy keys, not the v2 ones.
        assert_eq!(j["_sc_base_url"], "https://sc.test");
        assert_eq!(j["_sc_needs_m3u8_extraction"], true);
        assert!(j.get("base_url").is_none());
        assert!(j.get("needs_m3u8_extraction").is_none());
        assert!(j.get("title_id").is_none());
        assert_eq!(j["id"], "sc_9_77");
        assert_eq!(j["title"], "Serie S01E02 - Pilota");
        assert_eq!(j["url"], "https://sc.test/it/watch/9?e=77");
        assert_eq!(j["webpage_url"], j["url"]);
        assert_eq!(j["_type"], "video");
        assert_eq!(j["season_number"], 1);
        assert_eq!(j["episode_number"], 2);
        assert_eq!(j["episode"], "Pilota");
        assert_eq!(j["series"], "Serie");
    }

    #[test]
    fn a_movie_sidecar_nulls_the_series_fields_exactly_as_legacy_did() {
        let j = ScState::movie("https://sc.test", Some(5)).to_legacy_info_json(
            "sc_5",
            "Un film",
            "https://sc.test/it/watch/5",
        );
        assert_eq!(j["season_number"], Value::Null);
        assert_eq!(j["episode_number"], Value::Null);
        assert_eq!(j["series"], Value::Null);
        assert_eq!(j["episode"], "");
    }

    #[test]
    fn imported_legacy_keys_survive_into_the_sidecar_but_never_shadow_a_v2_key() {
        let mut s = ScState::movie("https://sc.test", Some(5));
        s.legacy
            .insert("plot".to_owned(), Value::String("Trama".to_owned()));
        s.legacy
            .insert("title".to_owned(), Value::String("stale".to_owned()));
        let j = s.to_legacy_info_json("sc_5", "Un film", "https://sc.test/it/watch/5");
        assert_eq!(j["plot"], "Trama");
        assert_eq!(j["title"], "Un film", "the v2 title must win");
        // And the blob keeps them.
        assert_eq!(s.to_json()["legacy"]["plot"], "Trama");
    }

    #[test]
    fn an_imported_blob_without_the_optional_fields_still_deserialises() {
        let v = serde_json::json!({
            "base_url": "https://sc.test",
            "needs_m3u8_extraction": true,
            "ext": "mp4",
            "extractor": "streamingcommunity",
            "extractor_key": "StreamingCommunity",
        });
        let s = ScState::from_json(&v).expect("a minimal blob must load");
        assert_eq!(s.title_id, None);
        assert_eq!(s.episode, "");
        assert_eq!(s.series, None);
    }
}
