//! The navigation model against a mock Apple Music API: back and forward, the page cache and
//! its invalidation, a load cancelled by moving on, and search hints.

mod pages_common;

use std::sync::Arc;
use std::time::Duration;

use pages_common::{EDGE, Env, env, fixture, json};
use rustet_app::nav::{Hints, Nav, NavState, PageState};
use rustet_app::pages::{Page, Recovery};
use rustet_app::route::Route;
use tokio::runtime::Handle;
use tokio::sync::watch;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

const ALBUM: &str = "1889568617";

fn album() -> Route {
    Route::Album { id: ALBUM.into() }
}

fn settings() -> Route {
    Route::Settings { section: None }
}

async fn until(nav: &Nav, what: &str, f: impl Fn(&NavState) -> bool) -> NavState {
    let mut rx = nav.state();
    tokio::time::timeout(Duration::from_secs(5), rx.wait_for(|s| f(s)))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for {what}: {:?}",
                nav.state().borrow().route
            )
        })
        .unwrap()
        .clone()
}

fn ready(state: &NavState) -> Option<Arc<Page>> {
    match &state.page {
        PageState::Ready(page) => Some(page.clone()),
        _ => None,
    }
}

async fn nav_at(env: &Env, start: Route) -> (Nav, watch::Sender<u32>) {
    let (session, rx) = watch::channel(0);
    (
        Nav::new(env.api.clone(), Handle::current(), start, rx),
        session,
    )
}

#[tokio::test]
async fn back_and_forward_show_cached_pages_at_once() {
    let env = env().await;
    env.get(
        &format!("/v1/catalog/us/albums/{ALBUM}"),
        fixture("album.json"),
    )
    .await;
    let (nav, _session) = nav_at(&env, album()).await;
    assert_eq!(nav.state().borrow().page, PageState::Loading);
    let first = ready(&until(&nav, "the album", |s| ready(s).is_some()).await).unwrap();

    nav.go(settings());
    let s = nav.state().borrow().clone();
    assert_eq!(
        (s.route.clone(), s.page.clone()),
        (settings(), PageState::Local)
    );
    assert!(s.can_back && !s.can_forward);

    nav.back();
    let s = nav.state().borrow().clone();
    assert_eq!(s.route, album());
    let again = ready(&s).expect("a cached page is shown without loading");
    assert!(Arc::ptr_eq(&first, &again));
    assert!(!s.can_back && s.can_forward);

    nav.forward();
    assert_eq!(nav.state().borrow().route, settings());
    nav.back();
    nav.go(Route::Downloads);
    let s = nav.state().borrow().clone();
    assert!(
        s.can_back && !s.can_forward,
        "going somewhere new clears forward"
    );
}

#[tokio::test]
async fn invalidated_pages_reload_and_a_session_change_empties_the_cache() {
    let env = env().await;
    env.get(
        &format!("/v1/catalog/us/albums/{ALBUM}"),
        fixture("album.json"),
    )
    .await;
    let (nav, session) = nav_at(&env, album()).await;
    let first = ready(&until(&nav, "the album", |s| ready(s).is_some()).await).unwrap();

    // Not the shown page: nothing reloads.
    nav.invalidate(|r| matches!(r, Route::Playlist { .. }));
    assert!(Arc::ptr_eq(&ready(&nav.state().borrow()).unwrap(), &first));

    // The shown page reloads in place: it stays on screen meanwhile.
    nav.invalidate(|r| *r == album());
    assert!(
        ready(&nav.state().borrow()).is_some(),
        "the old page stays while reloading"
    );
    let second = ready(
        &until(&nav, "the reload", |s| {
            ready(s).is_some_and(|p| !Arc::ptr_eq(&p, &first))
        })
        .await,
    )
    .unwrap();

    // A sign-in empties the cache: Back loads again.
    nav.go(settings());
    session.send_replace(1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    nav.back();
    assert_eq!(
        nav.state().borrow().page,
        PageState::Loading,
        "not cached any more"
    );
    let third = ready(&until(&nav, "the album again", |s| ready(s).is_some()).await).unwrap();
    assert!(!Arc::ptr_eq(&second, &third));
}

#[tokio::test]
async fn a_library_page_shown_at_sign_out_asks_to_sign_in() {
    let env = pages_common::signed_in().await;
    env.get("/v1/me/library/songs", r#"{"data":[]}"#).await;
    let route = Route::Library(rustet_app::route::LibrarySection::Songs);
    let (nav, session) = nav_at(&env, route).await;
    until(&nav, "the library", |s| ready(s).is_some()).await;
    env.api.set_user_token(None);
    session.send_replace(1);
    let s = until(&nav, "the sign-in error", |s| {
        matches!(s.page, PageState::Failed(_))
    })
    .await;
    assert!(matches!(s.page, PageState::Failed(e) if e.action == Recovery::SignIn));
}

#[tokio::test]
async fn moving_on_cancels_a_load_and_its_result_never_shows() {
    let env = env().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/catalog/us/albums/{ALBUM}")))
        .respond_with(json(fixture("album.json")).set_delay(Duration::from_millis(500)))
        .mount(&env.server)
        .await;
    let (nav, _session) = nav_at(&env, album()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    nav.go(settings());
    tokio::time::sleep(Duration::from_millis(700)).await;
    let s = nav.state().borrow().clone();
    assert_eq!((s.route, s.page), (settings(), PageState::Local));
    nav.back();
    assert_eq!(
        nav.state().borrow().page,
        PageState::Loading,
        "a cancelled load is not cached"
    );
    until(&nav, "the album", |s| ready(s).is_some()).await;
}

#[tokio::test]
async fn a_failed_load_says_what_to_do() {
    let env = env().await;
    Mock::given(method("GET"))
        .and(path("/v1/catalog/us/albums/404"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&env.server)
        .await;
    let (nav, _session) = nav_at(&env, Route::Album { id: "404".into() }).await;
    let s = until(&nav, "the error", |s| {
        matches!(s.page, PageState::Failed(_))
    })
    .await;
    let PageState::Failed(error) = s.page else {
        unreachable!()
    };
    assert_eq!(error.action, Recovery::GoBack);
    assert_eq!(error.title, "Rustet could not load this album.");
}

#[tokio::test]
async fn hints_wait_for_a_pause_and_drop_stale_answers() {
    let env = env().await;
    let route = format!("{EDGE}/v1/catalog/us/search/suggestions");
    let empty = r#"{"results":{"suggestions":[]}}"#;
    Mock::given(method("GET"))
        .and(path(&route))
        .and(query_param("term", "ph"))
        .respond_with(json(fixture("suggest.json")).set_delay(Duration::from_millis(600)))
        .mount(&env.server)
        .await;
    Mock::given(method("GET"))
        .and(path(&route))
        .and(query_param("term", "pho"))
        .respond_with(json(empty))
        .mount(&env.server)
        .await;
    Mock::given(method("GET"))
        .and(path(&route))
        .and(query_param("term", "phoebe"))
        .respond_with(json(fixture("suggest.json")))
        .mount(&env.server)
        .await;
    let (nav, _session) = nav_at(&env, settings()).await;
    let mut hints = nav.hints();

    // Typing fast: one request, for the last term.
    for term in ["p", "ph", "pho", "phoe", "phoeb", "phoebe"] {
        nav.search_input(term);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let h = tokio::time::timeout(
        Duration::from_secs(2),
        hints.wait_for(|h| !h.hints.is_empty()),
    )
    .await
    .unwrap()
    .unwrap()
    .clone();
    assert_eq!(h.term, "phoebe");
    let requests = env.server.received_requests().await.unwrap();
    assert_eq!(requests.iter().filter(|r| r.url.path() == route).count(), 1);

    // "ph" is slow and "pho" fast: the late answer for "ph" is dropped.
    nav.search_input("ph");
    tokio::time::sleep(Duration::from_millis(300)).await;
    nav.search_input("pho");
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(
        *nav.hints().borrow(),
        Hints {
            term: "pho".into(),
            hints: Vec::new()
        }
    );

    nav.search_input(" ");
    assert_eq!(nav.hints().borrow().hints, Vec::new());
}
