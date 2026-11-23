//! Checks a real sign-in without showing the token: reads the `signin` example's JSON line on
//! stdin, prints the token's length, and asks Apple Music's API for `/v1/me/storefront` with
//! the scraped web developer token and that user token.
//!
//!   signin PROFILE_DIR | check_token
//!
//! Prints, for example:
//!   result: token (source cookie, storefront Some("us"), token length 300)
//!   GET /v1/me/storefront: 200 OK
use std::io;
use std::process::ExitCode;

use rustet_signin::SignInResult;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut line = String::new();
    if let Err(error) = io::stdin().read_line(&mut line) {
        eprintln!("stdin: {error}");
        return ExitCode::FAILURE;
    }
    let token = match serde_json::from_str::<SignInResult>(&line) {
        Ok(SignInResult::Token {
            music_user_token,
            storefront,
            source,
        }) => {
            println!(
                "result: token (source {source:?}, storefront {storefront:?}, token length {})",
                music_user_token.len()
            );
            music_user_token
        }
        // Debug of the other results carries no token.
        Ok(other) => {
            println!("result: {other:?}");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            println!("not a sign-in result: {error}");
            return ExitCode::FAILURE;
        }
    };

    let http = reqwest::Client::new();
    let developer_token = match rustet_ampapi::Client::new(http.clone()).token().await {
        Ok(token) => token,
        Err(error) => {
            println!("developer token: {error}");
            return ExitCode::FAILURE;
        }
    };
    let response = http
        .get(format!("{}/v1/me/storefront", rustet_ampapi::AMP_BASE))
        .bearer_auth(developer_token)
        .header("music-user-token", token)
        .header("origin", rustet_ampapi::WEB_ORIGIN)
        .send()
        .await;
    match response {
        Ok(response) => {
            println!("GET /v1/me/storefront: {}", response.status());
            if response.status().is_success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            println!("GET /v1/me/storefront: {error}");
            ExitCode::FAILURE
        }
    }
}
