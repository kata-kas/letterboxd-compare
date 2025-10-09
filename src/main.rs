#[macro_use]
extern crate lazy_static;

mod cache;
mod letterboxd;

use crate::cache::*;
use crate::letterboxd::*;
use anyhow::Result;
use askama::Template;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::try_join;
use tracing::{debug, info};
use warp::Filter;
use warp::http::Response;
use sentry_panic;

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate<'a> {
    error_mess: Option<&'a str>,
}

#[derive(Template)]
#[template(path = "components/card.html")]
struct CardTemplate<'a> {
    movie: &'a Film,
    ratings: Vec<(&'a str, Option<Rating>)>,
}

#[derive(Template)]
#[template(path = "diff.html")]
struct DiffTemplate<'a> {
    user1: &'a str,
    user2: &'a str,
    cards: Vec<CardTemplate<'a>>,
}

#[derive(Template)]
#[template(path = "and.html")]
struct AndTemplate<'a> {
    user1: &'a str,
    user2: &'a str,
    cards: Vec<CardTemplate<'a>>,
}

lazy_static! {
    static ref CLIENT: LetterboxdClient = LetterboxdClient::new().unwrap();
}

async fn handle_vs(
    cache: Arc<RedisCache>,
    user1: String,
    user2: String,
) -> Result<warp::reply::Html<String>, warp::reject::Rejection> {
    let user1 = user1.trim().to_string();
    let user2 = user2.trim().to_string();
    match get_diff(&cache, &user1, &user2).await {
        Ok(s) => Ok(warp::reply::html(s)),
        Err(err) => {
            debug!("{:?}", &err);
            match (IndexTemplate {
                error_mess: Some(&err.to_string()),
            })
                .render()
            {
                Ok(html) => Ok(warp::reply::html(html)),
                Err(e) => {
                    tracing::error!("Template render error: {}", e);
                    Ok(warp::reply::html("Internal server error".to_string()))
                }
            }
        }
    }
}

async fn handle_and(
    cache: Arc<RedisCache>,
    user1: String,
    user2: String,
) -> Result<warp::reply::Html<String>, warp::reject::Rejection> {
    let user1 = user1.trim().to_string();
    let user2 = user2.trim().to_string();
    match get_and(&cache, &user1, &user2).await {
        Ok(s) => Ok(warp::reply::html(s)),
        Err(err) => {
            debug!("{:?}", &err);
            match (IndexTemplate {
                error_mess: Some(&err.to_string()),
            })
                .render()
            {
                Ok(html) => Ok(warp::reply::html(html)),
                Err(e) => {
                    tracing::error!("Template render error: {}", e);
                    Ok(warp::reply::html("Internal server error".to_string()))
                }
            }
        }
    }
}

async fn handle_health(
    cache: Arc<RedisCache>,
) -> Result<warp::reply::Json, warp::reject::Rejection> {
    // Check cache health by trying to get a non-existent key
    match cache.get("__health_check__").await {
        Ok(_) => Ok(warp::reply::json(&serde_json::json!({"status": "healthy"}))),
        Err(e) => {
            tracing::error!("Cache health check failed: {}", e);
            Ok(warp::reply::json(
                &serde_json::json!({"status": "unhealthy", "error": e.to_string()}),
            ))
        }
    }
}

async fn handle_poster(
    slug: String,
) -> Result<warp::http::Response<Vec<u8>>, warp::reject::Rejection> {
    let api_url = format!("https://letterboxd.com/film/{}/poster/std/230/", slug);
    let resp: reqwest::Response = CLIENT
        .client
        .get(&api_url)
        .send()
        .await
        .map_err(|_| warp::reject::not_found())?;
    let json: serde_json::Value = resp.json().await.map_err(|_| warp::reject::not_found())?;
    let image_url: &str = json["url2x"].as_str().ok_or(warp::reject::not_found())?;
    let image_resp: reqwest::Response = CLIENT
        .client
        .get(image_url)
        .send()
        .await
        .map_err(|_| warp::reject::not_found())?;
    let content_type: String = image_resp
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();
    let bytes = image_resp
        .bytes()
        .await
        .map_err(|_| warp::reject::not_found())?;
    let response = Response::builder()
        .header("content-type", content_type)
        .body(bytes.to_vec())
        .unwrap();
    Ok(response)
}

async fn cached_get_movies(cache: &RedisCache, username: &str) -> Result<Vec<Film>> {
    if let Some(s) = cache.get(username).await? {
        info!("cache hit for {}", username);
        let ret: Vec<Film> = serde_json::from_str(&s)?;
        return Ok(ret);
    }
    let movies = CLIENT.get_movies_of_user(username).await?;
    cache
        .insert(username, &serde_json::to_string(&movies)?)
        .await?;
    Ok(movies)
}

async fn get_diff(cache: &RedisCache, user1: &str, user2: &str) -> Result<String> {
    info!("get_diff({}, {})", user1, user2);
    let (movies1, movies2) = try_join!(
        cached_get_movies(cache, user1),
        cached_get_movies(cache, user2)
    )?;

    let watched_by_2: HashSet<_> = movies2.into_iter().map(|x| x.id).collect();

    let mut diff: Vec<_> = movies1
        .into_iter()
        .filter(|x| !watched_by_2.contains(&x.id))
        .collect();
    diff.sort_by(|a, b| a.rating.partial_cmp(&b.rating).unwrap().reverse());

    let cards: Vec<CardTemplate> = diff
        .iter()
        .map(|movie| CardTemplate {
            movie,
            ratings: vec![(user1, movie.rating)],
        })
        .collect();
    let html = DiffTemplate {
        user1,
        user2,
        cards,
    }
        .render()
        .map_err(|e| anyhow::anyhow!("Diff template error: {}", e))?;
    Ok(html)
}

async fn get_and(cache: &RedisCache, user1: &str, user2: &str) -> Result<String> {
    info!("get_and({}, {})", user1, user2);
    let (movies1, movies2) = try_join!(
        cached_get_movies(cache, user1),
        cached_get_movies(cache, user2)
    )?;

    let watched_by_2: HashSet<_> = movies2.into_iter().collect();

    let mut diff: Vec<_> = movies1
        .into_iter()
        .filter_map(|film| {
            watched_by_2
                .get(&film)
                .map(|film_user2| (film, film_user2.rating))
        })
        .collect();
    diff.sort_by(|a, b| match a.0.rating.cmp(&b.0.rating).reverse() {
        // If the rating of movie i and i+1 are equal for user 1, then
        // sort by the rating of user 2.
        Ordering::Equal => a.1.cmp(&b.1).reverse(),
        other => other,
    });

    let cards: Vec<CardTemplate> = diff
        .iter()
        .map(|(movie, other_rating)| CardTemplate {
            movie,
            ratings: vec![(user1, movie.rating), (user2, *other_rating)],
        })
        .collect();
    let html = AndTemplate {
        user1,
        user2,
        cards,
    }
        .render()
        .map_err(|e| anyhow::anyhow!("And template error: {}", e))?;
    Ok(html)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _guard = match std::env::var("SENTRY_DSN") {
        Ok(dsn) => {
            let guard = sentry::init((
                dsn,
                sentry::ClientOptions {
                    release: sentry::release_name!(),
                    send_default_pii: true,
                    ..Default::default()
                },
            ));
            std::panic::set_hook(Box::new(|info| {
               sentry_panic::panic_handler(info);
            }));
            Some(guard)
        },
        Err(e) => {
            tracing::error!("SENTRY_DSN not set or couldn't be read: {}", e);
            None
        }
    };

    tracing_subscriber::fmt::init();
    let cache = Arc::new(RedisCache::new().await?);
    let port = std::env::var("PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(3030);

    let versus = {
        let cache_clone = cache.clone();
        warp::path!(String / "vs" / String)
            .and_then(move |user1, user2| handle_vs(cache_clone.clone(), user1, user2))
    };
    let same = {
        let cache_clone = cache.clone();
        warp::path!(String / "and" / String)
            .and_then(move |user1, user2| handle_and(cache_clone.clone(), user1, user2))
    };
    let health = {
        let cache_clone = cache.clone();
        warp::path("health")
            .and(warp::get())
            .and_then(move || handle_health(cache_clone.clone()))
    };
    let poster = warp::path!("poster" / String).and_then(handle_poster);
    let index = warp::path::end().map(|| -> warp::reply::Html<String> {
        let html = IndexTemplate { error_mess: None }
            .render()
            .unwrap_or_else(|e| {
                tracing::error!("Index template render error: {}", e);
                "Internal server error".to_string()
            });
        warp::reply::html(html)
    });

    let routes = health.or(versus).or(same).or(poster).or(index);

    warp::serve(routes).run(([0, 0, 0, 0], port)).await;

    Ok(())
}
