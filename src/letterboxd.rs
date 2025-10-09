use anyhow::{anyhow, Context, Result};
use core::hash::{Hash, Hasher};
use futures::{stream, StreamExt, TryStreamExt};
use governor::state::{InMemoryState, NotKeyed};
use governor::{clock::DefaultClock, Jitter, Quota, RateLimiter};
use lazy_static::lazy_static;
use reqwest::{Client, ClientBuilder, StatusCode};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::cmp::{Eq, Ord, PartialEq, PartialOrd};
use std::fmt::Formatter;
use std::num::NonZeroU32;
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, info};

const BASE_URL: &str = "https://letterboxd.com";
const MAX_RETRIES: usize = 3;
const CONCURRENT_REQUESTS: usize = 5;
const REQUESTS_PER_MINUTE: u32 = 60;

lazy_static! {
    static ref DATA_SELECTOR: Selector = Selector::parse("div[data-film-id]").unwrap();
    static ref POSTER_SELECTOR: Selector = Selector::parse("div.poster.film-poster img").unwrap();
    static ref RATING_SELECTOR: Selector = Selector::parse("span.rating").unwrap();
    static ref PAGINATION_SELECTOR: Selector = Selector::parse("div.pagination").unwrap();
    static ref PAGINATE_PAGE_SELECTOR: Selector = Selector::parse("li.paginate-page > a").unwrap();
    static ref GRID_ITEM_SELECTOR: Selector = Selector::parse("li.griditem").unwrap();
}

#[derive(Error, Debug)]
pub enum LetterboxdError {
    #[error("missing HTML attribute: {0}")]
    HtmlMissingAttr(String),

    #[error("user not found: {0}")]
    UserNotFound(String),

    #[error("pagination element not found")]
    PaginationElementNotFound,

    #[error("invalid username: {0}")]
    InvalidUsername(String),

    #[error("parsing failed: {0}")]
    ParseError(#[from] std::num::ParseIntError),

    #[error("network error: {0}")]
    NetworkError(#[from] reqwest::Error),

    #[error("rate limit exceeded")]
    RateLimitExceeded,
}

#[derive(Copy, Clone, Serialize, Deserialize, Ord, PartialOrd, Eq, PartialEq)]
pub struct Rating(u8);

impl std::fmt::Debug for Rating {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", (self.0 as f64) / 2.0)
    }
}

impl std::fmt::Display for Rating {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let star = match self.0 {
            1 => "½",
            2 => "★",
            3 => "★½",
            4 => "★★",
            5 => "★★½",
            6 => "★★★",
            7 => "★★★½",
            8 => "★★★★",
            9 => "★★★★½",
            10 => "★★★★★",
            _ => {
                debug!("Invalid rating value: {}", self.0);
                "no rating"
            }
        };
        write!(f, "{}", star)
    }
}

impl TryFrom<u8> for Rating {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        if value <= 10 {
            Ok(Rating(value))
        } else {
            Err(anyhow!("Invalid rating value: {}", value))
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Eq)]
pub struct Film {
    pub id: u64,
    pub name: String,
    pub url: String,
    pub poster: String,
    pub rating: Option<Rating>,
}

impl Hash for Film {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state)
    }
}

impl PartialEq for Film {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

/// Configuration for LetterboxdClient
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub user_agent: String,
    pub timeout: Duration,
    pub max_retries: usize,
    pub concurrent_requests: usize,
    pub requests_per_minute: u32,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36".to_string(),
            timeout: Duration::from_secs(30),
            max_retries: MAX_RETRIES,
            concurrent_requests: CONCURRENT_REQUESTS,
            requests_per_minute: REQUESTS_PER_MINUTE,
        }
    }
}

pub struct LetterboxdClient {
    pub(crate) client: Client,
    rate_limiter: RateLimiter<NotKeyed, InMemoryState, DefaultClock>,
    config: ClientConfig,
}

impl LetterboxdClient {
    /// Creates a new LetterboxdClient with the provided configuration
    pub fn new_with_config(config: ClientConfig) -> Result<Self> {
        let client = ClientBuilder::new()
            .user_agent(&config.user_agent)
            .timeout(config.timeout)
            .gzip(true)
            .build()
            .context("Failed to build reqwest client")?;

        let quota = Quota::per_minute(NonZeroU32::new(config.requests_per_minute).unwrap());
        let rate_limiter = RateLimiter::direct(quota);

        Ok(LetterboxdClient {
            client,
            rate_limiter,
            config,
        })
    }

    /// Creates a new LetterboxdClient with default configuration
    pub fn new() -> Result<Self> {
        Self::new_with_config(ClientConfig::default())
    }

    /// Validates a username
    fn validate_username(username: &str) -> Result<()> {
        if username.trim().is_empty() {
            return Err(
                LetterboxdError::InvalidUsername("Username cannot be empty".to_string()).into(),
            );
        }
        if !username
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            return Err(LetterboxdError::InvalidUsername(
                "Username contains invalid characters".to_string(),
            )
            .into());
        }
        Ok(())
    }

    /// Parses star rating from text to Rating struct
    fn parse_rating(rating: &str) -> Result<Rating> {
        let value = match rating.trim() {
            "½" => 1,
            "★" => 2,
            "★½" => 3,
            "★★" => 4,
            "★★½" => 5,
            "★★★" => 6,
            "★★★½" => 7,
            "★★★★" => 8,
            "★★★★½" => 9,
            "★★★★★" => 10,
            _ => return Err(anyhow!("Unknown rating format: '{}'", rating)),
        };
        Rating::try_from(value).context("Failed to convert rating value")
    }

    /// Gets the number of pages for a user's film list
    fn get_pages(&self, html: &Html) -> Result<usize> {
        let page = match html.select(&PAGINATION_SELECTOR).next() {
            Some(page) => page,
            None => return Ok(1),
        };
        let no_pages = page
            .select(&PAGINATE_PAGE_SELECTOR)
            .last()
            .ok_or(LetterboxdError::PaginationElementNotFound)?
            .inner_html()
            .parse()
            .context("Failed to parse page number")?;
        Ok(no_pages)
    }

    /// Converts HTML element to Film struct
    fn film_from_elem_ref(&self, movie: &scraper::ElementRef) -> Result<Film> {
        let data = movie
            .select(&DATA_SELECTOR)
            .next()
            .ok_or_else(|| LetterboxdError::HtmlMissingAttr("data-film-id".into()))?
            .value();

        let poster = movie
            .select(&POSTER_SELECTOR)
            .next()
            .ok_or_else(|| LetterboxdError::HtmlMissingAttr("poster".into()))?
            .value();

        let rating = movie
            .select(&RATING_SELECTOR)
            .next()
            .and_then(|r| r.text().next())
            .map(Self::parse_rating)
            .transpose()
            .context("Failed to parse rating")?;

        let slug = data
            .attr("data-item-slug")
            .ok_or_else(|| LetterboxdError::HtmlMissingAttr("data-item-slug".into()))?;

        Ok(Film {
            id: data
                .attr("data-film-id")
                .ok_or_else(|| LetterboxdError::HtmlMissingAttr("data-film-id".into()))?
                .parse()
                .context("Failed to parse film ID")?,
            name: poster
                .attr("alt")
                .ok_or_else(|| LetterboxdError::HtmlMissingAttr("alt".into()))?
                .to_string(),
            url: format!("{}/film/{}", BASE_URL, slug),
            poster: format!("/poster/{}", slug),
            rating,
        })
    }

    /// Fetches a single page of films for a user with retry mechanism
    #[tracing::instrument(skip(self))]
    async fn get_letterboxd_film_by_page(
        &self,
        username: &str,
        page: usize,
    ) -> Result<reqwest::Response> {
        LetterboxdClient::validate_username(username)?;
        let url = format!("{}/{}/films/page/{}", BASE_URL, username, page);
        debug!("Fetching URL: {}", url);

        self.rate_limiter
            .until_ready_with_jitter(Jitter::up_to(Duration::from_millis(100)))
            .await;

        let mut last_error = None;
        for attempt in 1..=self.config.max_retries {
            match self.client.get(&url).send().await {
                Ok(response) => match response.status() {
                    StatusCode::OK => return Ok(response),
                    StatusCode::NOT_FOUND => {
                        return Err(LetterboxdError::UserNotFound(username.to_string()).into())
                    }
                    StatusCode::TOO_MANY_REQUESTS => {
                        return Err(LetterboxdError::RateLimitExceeded.into())
                    }
                    status => {
                        last_error = Some(anyhow!("Unexpected status code: {}", status));
                    }
                },
                Err(e) => {
                    last_error = Some(e.into());
                }
            }
            debug!(
                "Retry {}/{} for page {}",
                attempt, self.config.max_retries, page
            );
            tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow!(
                "Failed to fetch page after {} retries",
                self.config.max_retries
            )
        }))
    }

    /// Gets movies from a specific page
    #[tracing::instrument(skip(self))]
    pub async fn get_movies_from_page(&self, username: &str, page: usize) -> Result<Vec<Film>> {
        LetterboxdClient::validate_username(username)?;
        let text = self
            .get_letterboxd_film_by_page(username, page)
            .await?
            .text()
            .await
            .context("Failed to get response text")?;
        let document = Html::parse_document(&text);
        let films = document
            .select(&GRID_ITEM_SELECTOR)
            .map(|movie| self.film_from_elem_ref(&movie))
            .collect::<Result<Vec<_>>>()
            .context("Failed to parse films from page")?;

        Ok(films)
    }

    /// Gets all movies for a user across all pages
    #[tracing::instrument(skip(self))]
    pub async fn get_movies_of_user(&self, username: &str) -> Result<Vec<Film>> {
        LetterboxdClient::validate_username(username)?;

        let no_of_pages = {
            let resp = self
                .get_letterboxd_film_by_page(username, 1)
                .await
                .context("Failed to fetch first page")?;
            let text = resp.text().await.context("Failed to get response text")?;
            let document = Html::parse_document(&text);
            self.get_pages(&document)
                .context("Failed to get page count")?
        };

        info!(
            no_of_pages = no_of_pages,
            "Found pages for user {}", username
        );

        let films = stream::iter(1..=no_of_pages)
            .map(|i| self.get_movies_from_page(username, i))
            .buffer_unordered(self.config.concurrent_requests)
            .try_collect::<Vec<_>>()
            .await
            .context("Failed to collect films")?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();

        info!(
            films_len = films.len(),
            "Collected films for user {}", username
        );
        Ok(films)
    }
}

/// Unit tests
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rating_parse() {
        assert_eq!(LetterboxdClient::parse_rating("★").unwrap(), Rating(2));
        assert_eq!(LetterboxdClient::parse_rating("★★★★½").unwrap(), Rating(9));
        assert!(LetterboxdClient::parse_rating("invalid").is_err());
    }

    #[test]
    fn test_rating_display() {
        assert_eq!(Rating(2).to_string(), "★");
        assert_eq!(Rating(9).to_string(), "★★★★½");
        assert_eq!(Rating(11).to_string(), "no rating");
    }

    #[test]
    fn test_username_validation() {
        assert!(LetterboxdClient::validate_username("valid-user").is_ok());
        assert!(LetterboxdClient::validate_username("").is_err());
        assert!(LetterboxdClient::validate_username("invalid/user").is_err());
    }
}
