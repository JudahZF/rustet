//! "Load more": the continuation of a paged list, typed by what it yields.
//!
//! Apple's `next` links drop some of the first request's parameters. Search links then answer
//! with nothing (`rustet-ampapi` adds `with=…` back in `Api::search_next`); other lists lose
//! `include` and so their artist and album links. Each continuation here re-adds the
//! parameters its first request had.

use std::fmt;
use std::marker::PhantomData;

use rustet_ampapi::Api;
use rustet_ampapi::model::{Cursor, Page, Resource};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::error::LoadError;
use super::map;
use super::model::{List, Tile, TrackRow};

/// Something a list page yields.
pub trait ListItem: Sized {
    fn from_resource(resource: &Resource) -> Option<Self>;
}

impl ListItem for TrackRow {
    fn from_resource(resource: &Resource) -> Option<Self> {
        map::track(resource)
    }
}

impl ListItem for Tile {
    fn from_resource(resource: &Resource) -> Option<Self> {
        map::tile(resource)
    }
}

#[derive(Clone, PartialEq, Eq)]
enum Source {
    /// A `next` link, with the query pairs to add when it lacks them.
    Next {
        cursor: Cursor,
        params: &'static [(&'static str, &'static str)],
    },
    /// One group of search results (`song`, `album`, …).
    Search { cursor: Cursor, group: String },
    /// A catalog playlist's tracks from `offset` (`Api::playlist_tracks`, which includes the
    /// artists).
    PlaylistTracks { id: String, offset: u32 },
}

/// How to load the next part of a list of `T`. Loading it is one request; dropping the future
/// cancels it.
pub struct LoadMore<T> {
    source: Source,
    item: PhantomData<fn() -> T>,
}

impl<T> Clone for LoadMore<T> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            item: PhantomData,
        }
    }
}

impl<T> PartialEq for LoadMore<T> {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

impl<T> fmt::Debug for LoadMore<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            Source::Next { cursor, .. } => write!(f, "LoadMore({})", cursor.as_str()),
            Source::Search { cursor, group } => write!(f, "LoadMore({group}: {})", cursor.as_str()),
            Source::PlaylistTracks { id, offset } => write!(f, "LoadMore({id} from {offset})"),
        }
    }
}

impl<T> LoadMore<T> {
    fn new(source: Source) -> Self {
        Self {
            source,
            item: PhantomData,
        }
    }

    /// Continues with Apple's `next` link, adding `params` it lacks.
    pub(crate) fn next(
        cursor: Option<Cursor>,
        params: &'static [(&'static str, &'static str)],
    ) -> Option<Self> {
        cursor.map(|cursor| Self::new(Source::Next { cursor, params }))
    }

    pub(crate) fn search(cursor: Option<Cursor>, group: &str) -> Option<Self> {
        cursor.map(|cursor| {
            Self::new(Source::Search {
                cursor,
                group: group.to_owned(),
            })
        })
    }

    pub(crate) fn playlist_tracks(id: &str, offset: u32) -> Self {
        Self::new(Source::PlaylistTracks {
            id: id.to_owned(),
            offset,
        })
    }

    /// The Apple path this continues with (diagnostics, tests).
    pub fn describe(&self) -> String {
        format!("{self:?}")
    }
}

impl<T: ListItem> LoadMore<T> {
    /// The next part of the list. Needs the music user token when the list is the library's.
    pub async fn load(&self, api: &Api) -> Result<List<T>, LoadError> {
        let cancel = CancellationToken::new();
        let page: Page<Resource> = match &self.source {
            Source::Next { cursor, params } => {
                api.next_page(&cancel, &with_params(cursor, params)).await?
            }
            Source::Search { cursor, group } => {
                let results = api.search_next(&cancel, cursor).await?;
                let group = results.groups.into_iter().find(|g| &g.group_id == group);
                let more = group
                    .as_ref()
                    .and_then(|g| Self::search(g.next.clone(), &g.group_id));
                let items = group
                    .map(|g| g.data.iter().filter_map(T::from_resource).collect())
                    .unwrap_or_default();
                return Ok(List { items, more });
            }
            Source::PlaylistTracks { id, offset } => {
                let page = match api.playlist_tracks(&cancel, id, *offset).await {
                    // An offset past the end answers 404: the list is complete.
                    Err(rustet_ampapi::Error::NotFound) => Page::default(),
                    other => other?,
                };
                let next_offset = offset + page.data.len() as u32;
                let more = (page.next.is_some() && !page.data.is_empty())
                    .then(|| Self::playlist_tracks(id, next_offset));
                let items = page.data.iter().filter_map(T::from_resource).collect();
                return Ok(List { items, more });
            }
        };
        let more = match &self.source {
            Source::Next { params, .. } => Self::next(page.next, params),
            _ => None,
        };
        Ok(List {
            items: page.data.iter().filter_map(T::from_resource).collect(),
            more,
        })
    }
}

/// `cursor` with each pair of `params` whose key it lacks.
pub(crate) fn with_params(cursor: &Cursor, params: &[(&str, &str)]) -> Cursor {
    let Ok(mut url) = Url::parse(&format!("https://x{}", cursor.as_str())) else {
        return cursor.clone();
    };
    let present: Vec<String> = url.query_pairs().map(|(k, _)| k.into_owned()).collect();
    let missing: Vec<_> = params
        .iter()
        .filter(|(k, _)| !present.iter().any(|p| p == k))
        .collect();
    if missing.is_empty() {
        return cursor.clone();
    }
    url.query_pairs_mut().extend_pairs(missing);
    let path = format!("{}?{}", url.path(), url.query().unwrap_or_default());
    Cursor::new(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_params_are_added_and_present_ones_kept() {
        let c = with_params(
            &Cursor::new("/v1/me/library/songs?offset=100&include=catalog"),
            &[("include", "catalog,albums"), ("limit", "100")],
        );
        assert_eq!(
            c.as_str(),
            "/v1/me/library/songs?offset=100&include=catalog&limit=100"
        );
    }
}
