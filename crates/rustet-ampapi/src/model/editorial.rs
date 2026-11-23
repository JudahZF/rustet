//! Editorial pages as shelves the UI lays out itself.
//!
//! Apple's editorial pages are trees of `editorial-elements`; the meaning of a node is a
//! number in `editorialElementKind`. The kinds below were seen in the New, Radio and
//! curator pages (2026-11). Any other kind becomes [`Shelf::Unknown`] with whatever items
//! it holds, so the UI can draw it as a plain tile row and a new kind never fails a page.

use super::{EditorialElement, Grouping, Link, Page, Recommendation, Resource};
use crate::model::Artwork;

/// Kinds of editorial element (observed values of `editorialElementKind`).
mod kind {
    /// A large-tile carousel; each child (317, 320, 383) holds one item and a badge.
    pub const HERO: &str = "316";
    /// A curated group of large tiles; each child (394) holds one station.
    pub const FEATURED_GROUP: &str = "385";
    /// A row of station tiles; each child (489) holds one station.
    pub const STATION_ROW: &str = "488";
    /// A row of tiles (albums, playlists, stations, videos).
    pub const GRID: &str = "326";
    /// A list of songs.
    pub const TRACKS: &str = "327";
    /// A list of links ("More to Explore").
    pub const LINKS: &str = "391";
    /// A list of links without a title (genres).
    pub const LINKS_PLAIN: &str = "322";
}

/// An editorial page: the shelves of the page's first tab.
#[derive(Debug, Clone)]
pub struct EditorialPage {
    pub id: String,
    pub title: String,
    pub shelves: Vec<Shelf>,
}

impl From<Grouping> for EditorialPage {
    fn from(grouping: Grouping) -> Self {
        let shelves = grouping
            .relationships
            .tabs
            .and_then(|tabs| tabs.data.into_iter().next())
            .and_then(|tab| tab.relationships.children)
            .map(|children| shelves(children.data))
            .unwrap_or_default();
        Self {
            id: grouping.id,
            title: grouping.attributes.map(|a| a.name).unwrap_or_default(),
            shelves,
        }
    }
}

/// The shelves of a list of editorial elements; other resources are skipped.
pub(crate) fn shelves(elements: Vec<Resource>) -> Vec<Shelf> {
    elements
        .into_iter()
        .filter_map(|element| match element {
            Resource::EditorialElement(element) => Some(Shelf::from(*element)),
            _ => None,
        })
        .collect()
}

/// How Apple draws a tile row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayStyle {
    Compact,
    Expanded,
    /// Another value, for example a recommendation shelf's component name.
    Other(String),
    /// Apple gave none.
    Unspecified,
}

impl From<Option<String>> for DisplayStyle {
    fn from(style: Option<String>) -> Self {
        match style.as_deref() {
            None => Self::Unspecified,
            Some("compact") => Self::Compact,
            Some("expanded") => Self::Expanded,
            Some(_) => Self::Other(style.unwrap_or_default()),
        }
    }
}

/// One shelf of an editorial page, Home or the search landing page.
#[derive(Debug, Clone)]
pub enum Shelf {
    /// Large tiles, one item each, often with a badge ("NEW ALBUM").
    Featured {
        id: String,
        title: Option<String>,
        tiles: Vec<Tile>,
    },
    /// A row of tiles.
    Grid {
        id: String,
        title: Option<String>,
        style: DisplayStyle,
        items: Vec<Resource>,
        /// The "see all" room ([`Api::room`](crate::Api::room)).
        room: Option<String>,
    },
    /// A list of songs.
    Tracks {
        id: String,
        title: Option<String>,
        items: Vec<Resource>,
        room: Option<String>,
    },
    /// Text links.
    Links {
        id: String,
        title: Option<String>,
        links: Vec<Link>,
    },
    /// A kind this crate does not know, with every item it holds (its own and its
    /// children's). Draw it as a plain tile row.
    Unknown {
        id: String,
        kind: Option<String>,
        title: Option<String>,
        items: Vec<Resource>,
    },
}

impl Shelf {
    pub fn title(&self) -> Option<&str> {
        match self {
            Self::Featured { title, .. }
            | Self::Grid { title, .. }
            | Self::Tracks { title, .. }
            | Self::Links { title, .. }
            | Self::Unknown { title, .. } => title.as_deref(),
        }
    }
}

/// A large tile of a [`Shelf::Featured`].
#[derive(Debug, Clone)]
pub struct Tile {
    pub item: Resource,
    pub badge: Option<String>,
    /// Editorial text for the tile, when Apple gives one.
    pub caption: Option<String>,
    /// Tile-specific artwork; `None` means use the item's.
    pub artwork: Option<Artwork>,
}

fn contents(page: Option<Page<Resource>>) -> Vec<Resource> {
    page.map(|p| p.data).unwrap_or_default()
}

impl From<EditorialElement> for Shelf {
    fn from(element: EditorialElement) -> Self {
        let id = element.id;
        let attributes = element.attributes.unwrap_or_default();
        let relationships = element.relationships;
        let title = attributes.name.or(attributes.title);
        let room = relationships
            .room
            .and_then(|r| r.data.into_iter().next())
            .map(|r| r.id().to_owned());
        match attributes.editorial_element_kind.as_deref() {
            Some(kind::GRID) => Self::Grid {
                id,
                title,
                style: attributes.display_style.into(),
                items: contents(relationships.contents),
                room,
            },
            Some(kind::TRACKS) => Self::Tracks {
                id,
                title,
                items: contents(relationships.contents),
                room,
            },
            Some(kind::HERO | kind::FEATURED_GROUP | kind::STATION_ROW) => Self::Featured {
                id,
                title,
                tiles: contents(relationships.children)
                    .into_iter()
                    .filter_map(tile)
                    .collect(),
            },
            Some(kind::LINKS | kind::LINKS_PLAIN) => Self::Links {
                id,
                title,
                links: attributes.links,
            },
            _ => {
                let mut items = contents(relationships.contents);
                for child in contents(relationships.children) {
                    match child {
                        Resource::EditorialElement(child) => {
                            items.extend(contents(child.relationships.contents));
                        }
                        other => items.push(other),
                    }
                }
                Self::Unknown {
                    id,
                    kind: attributes.editorial_element_kind,
                    title,
                    items,
                }
            }
        }
    }
}

/// A child element holding one item, as a tile. Children without an item are dropped.
fn tile(child: Resource) -> Option<Tile> {
    let Resource::EditorialElement(child) = child else {
        return Some(Tile {
            item: child,
            badge: None,
            caption: None,
            artwork: None,
        });
    };
    let attributes = child.attributes.unwrap_or_default();
    let item = contents(child.relationships.contents).into_iter().next()?;
    Some(Tile {
        item,
        badge: attributes.design_badge,
        caption: attributes.name,
        artwork: attributes.artwork,
    })
}

impl From<Recommendation> for Shelf {
    /// A recommendation shelf is a tile row whose style is Apple's component name.
    fn from(recommendation: Recommendation) -> Self {
        let attributes = recommendation.attributes.unwrap_or_default();
        Self::Grid {
            id: recommendation.id,
            title: attributes.title.and_then(|t| t.string_for_display),
            style: attributes.display.and_then(|d| d.kind).into(),
            items: contents(recommendation.relationships.contents),
            room: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(json: &str) -> Shelf {
        Shelf::from(serde_json::from_str::<EditorialElement>(json).unwrap())
    }

    #[test]
    fn a_new_shelf_kind_keeps_its_items() {
        let shelf = element(
            r#"{"id":"9","attributes":{"editorialElementKind":"999","name":"New thing"},
                "relationships":{"children":{"data":[
                    {"id":"c","type":"editorial-elements","relationships":{"contents":{"data":[
                        {"id":"1","type":"albums","attributes":{"name":"A"}}]}}},
                    {"id":"2","type":"playlists","attributes":{"name":"P"}}]}}}"#,
        );
        let Shelf::Unknown {
            kind, title, items, ..
        } = shelf
        else {
            panic!("unknown shelf")
        };
        assert_eq!(kind.as_deref(), Some("999"));
        assert_eq!(title.as_deref(), Some("New thing"));
        let ids: Vec<_> = items.iter().map(Resource::id).collect();
        assert_eq!(ids, ["1", "2"]);
    }

    #[test]
    fn hero_children_become_tiles_and_empty_children_are_dropped() {
        let shelf = element(
            r#"{"id":"h","attributes":{"editorialElementKind":"316","displayStyle":"compact"},
                "relationships":{"children":{"data":[
                    {"id":"a","type":"editorial-elements","attributes":{"designBadge":"NEW ALBUM"},
                     "relationships":{"contents":{"data":[{"id":"1","type":"albums"}]}}},
                    {"id":"b","type":"editorial-elements","attributes":{}}]}}}"#,
        );
        let Shelf::Featured { tiles, .. } = shelf else {
            panic!("featured shelf")
        };
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].badge.as_deref(), Some("NEW ALBUM"));
        assert_eq!(tiles[0].item.id(), "1");
    }
}
