use crate::{Auth, Body, Endpoint, RestClient};
use http::Method;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct TraktAuth {
    pub client_id: String,
}

impl Auth for TraktAuth {
    fn apply(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("trakt-api-key", &self.client_id)
            .header("trakt-api-version", "2")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", "Mozilla/5.0 (compatible; remux/1.0)")
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraktItemIds {
    pub imdb: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraktPopularItem {
    pub ids: TraktItemIds,
}

#[derive(Debug, Clone, Serialize)]
pub struct PopularParams {
    pub limit: u32,
}

#[derive(Debug, Clone)]
pub struct MoviePopularEndpoint {
    pub limit: u32,
}

impl Endpoint for MoviePopularEndpoint {
    type Output = Vec<TraktPopularItem>;

    fn path(&self) -> String {
        "movies/popular".to_string()
    }

    fn query_params(&self) -> impl serde::Serialize + '_ {
        PopularParams { limit: self.limit }
    }
}

#[derive(Debug, Clone)]
pub struct ShowPopularEndpoint {
    pub limit: u32,
}

impl Endpoint for ShowPopularEndpoint {
    type Output = Vec<TraktPopularItem>;

    fn path(&self) -> String {
        "shows/popular".to_string()
    }

    fn query_params(&self) -> impl serde::Serialize + '_ {
        PopularParams { limit: self.limit }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraktStats {
    pub watchers: u64,
    pub recommended: u64,
    pub favorited: u64,
}

impl TraktStats {
    pub fn raw_score(&self) -> f64 {
        self.watchers as f64
            + self.recommended as f64 * 20.0
            + self.favorited as f64 * 10.0
    }
}

#[derive(Debug, Clone)]
pub struct MovieStatsEndpoint {
    pub imdb_id: String,
}

impl Endpoint for MovieStatsEndpoint {
    type Output = TraktStats;

    fn path(&self) -> String {
        format!("movies/{}/stats", self.imdb_id)
    }

    fn query_params(&self) -> impl serde::Serialize + '_ {
        ()
    }
}

#[derive(Debug, Clone)]
pub struct ShowStatsEndpoint {
    pub imdb_id: String,
}

impl Endpoint for ShowStatsEndpoint {
    type Output = TraktStats;

    fn path(&self) -> String {
        format!("shows/{}/stats", self.imdb_id)
    }

    fn query_params(&self) -> impl serde::Serialize + '_ {
        ()
    }
}

pub fn trakt_client(
    client_id: &str,
    base_url: &str,
) -> Result<RestClient<TraktAuth>, url::ParseError> {
    Ok(RestClient::new(base_url)?.with_auth(TraktAuth {
        client_id: client_id.to_string(),
    }))
}

#[derive(Clone, Debug)]
pub struct TraktUserAuth {
    pub client_id: String,
    pub access_token: String,
}

impl Auth for TraktUserAuth {
    fn apply(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("trakt-api-key", &self.client_id)
            .header("trakt-api-version", "2")
            .header("User-Agent", "Mozilla/5.0 (compatible; remux/1.0)")
            .bearer_auth(&self.access_token)
    }
}

pub fn trakt_user_client(
    client_id: &str,
    access_token: &str,
    base_url: &str,
) -> Result<RestClient<TraktUserAuth>, url::ParseError> {
    Ok(RestClient::new(base_url)?.with_auth(TraktUserAuth {
        client_id: client_id.to_string(),
        access_token: access_token.to_string(),
    }))
}

#[derive(Debug, Clone, Serialize)]
pub struct TraktIdsRef {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imdb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmdb: Option<i64>,
}

#[derive(Debug, Clone)]
pub enum ScrobbleTarget {
    Movie {
        ids: TraktIdsRef,
    },
    Episode {
        show_ids: TraktIdsRef,
        season: i64,
        number: i64,
    },
}

impl ScrobbleTarget {
    fn to_body(&self, progress: f64) -> serde_json::Value {
        match self {
            ScrobbleTarget::Movie { ids } => serde_json::json!({
                "movie": { "ids": ids },
                "progress": progress,
            }),
            ScrobbleTarget::Episode {
                show_ids,
                season,
                number,
            } => serde_json::json!({
                "show": { "ids": show_ids },
                "episode": { "season": season, "number": number },
                "progress": progress,
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScrobbleStartEndpoint {
    pub target: ScrobbleTarget,
    pub progress: f64,
}

impl Endpoint for ScrobbleStartEndpoint {
    type Output = serde_json::Value;
    fn path(&self) -> String {
        "scrobble/start".to_string()
    }
    fn method(&self) -> Method {
        Method::POST
    }
    fn body(&self) -> Body {
        Body::Json(self.target.to_body(self.progress))
    }
}

#[derive(Debug, Clone)]
pub struct ScrobblePauseEndpoint {
    pub target: ScrobbleTarget,
    pub progress: f64,
}

impl Endpoint for ScrobblePauseEndpoint {
    type Output = serde_json::Value;
    fn path(&self) -> String {
        "scrobble/pause".to_string()
    }
    fn method(&self) -> Method {
        Method::POST
    }
    fn body(&self) -> Body {
        Body::Json(self.target.to_body(self.progress))
    }
}

#[derive(Debug, Clone)]
pub struct ScrobbleStopEndpoint {
    pub target: ScrobbleTarget,
    pub progress: f64,
}

impl Endpoint for ScrobbleStopEndpoint {
    type Output = serde_json::Value;
    fn path(&self) -> String {
        "scrobble/stop".to_string()
    }
    fn method(&self) -> Method {
        Method::POST
    }
    fn body(&self) -> Body {
        Body::Json(self.target.to_body(self.progress))
    }
}

#[cfg(test)]
mod scrobble_tests {
    use super::*;

    #[test]
    fn trakt_user_auth_sets_expected_headers() {
        let auth = TraktUserAuth {
            client_id: "cid".to_string(),
            access_token: "tok".to_string(),
        };
        let client = reqwest::Client::new();
        let req = auth
            .apply(client.get("http://example.com"))
            .build()
            .unwrap();

        assert_eq!(req.headers().get("trakt-api-key").unwrap(), "cid");
        assert_eq!(req.headers().get("trakt-api-version").unwrap(), "2");
        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer tok");
    }

    #[test]
    fn scrobble_start_movie_body_shape() {
        let ep = ScrobbleStartEndpoint {
            target: ScrobbleTarget::Movie {
                ids: TraktIdsRef {
                    imdb: Some("tt123".to_string()),
                    tmdb: None,
                },
            },
            progress: 42.5,
        };
        let Body::Json(json) = ep.body() else {
            panic!("expected json body")
        };
        assert_eq!(
            json,
            serde_json::json!({
                "movie": { "ids": { "imdb": "tt123" } },
                "progress": 42.5,
            })
        );
        assert_eq!(ep.path(), "scrobble/start");
        assert_eq!(ep.method(), http::Method::POST);
    }

    #[test]
    fn scrobble_stop_episode_body_shape() {
        let ep = ScrobbleStopEndpoint {
            target: ScrobbleTarget::Episode {
                show_ids: TraktIdsRef {
                    imdb: Some("tt999".to_string()),
                    tmdb: None,
                },
                season: 2,
                number: 5,
            },
            progress: 91.0,
        };
        let Body::Json(json) = ep.body() else {
            panic!("expected json body")
        };
        assert_eq!(
            json,
            serde_json::json!({
                "show": { "ids": { "imdb": "tt999" } },
                "episode": { "season": 2, "number": 5 },
                "progress": 91.0,
            })
        );
        assert_eq!(ep.path(), "scrobble/stop");
    }

    #[test]
    fn scrobble_pause_uses_pause_path() {
        let ep = ScrobblePauseEndpoint {
            target: ScrobbleTarget::Movie {
                ids: TraktIdsRef { imdb: None, tmdb: Some(42) },
            },
            progress: 10.0,
        };
        assert_eq!(ep.path(), "scrobble/pause");
    }
}
