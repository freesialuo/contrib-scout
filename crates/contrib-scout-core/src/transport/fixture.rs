//! Scripted transport for tests (§13: no real requests in CI).
//!
//! Exchanges are answered strictly in FIFO order. Each may pin the expected
//! path; a mismatch, or a request with nothing scripted, is an error (so tests
//! notice unexpected traffic). Every request is recorded, including failed ones.
//!
//! ```
//! use contrib_scout_core::transport::{ApiRequest, ApiResponse, FixtureTransport, Transport};
//! let fixture = FixtureTransport::new();
//! fixture.expect_get("/repos/o/r", ApiResponse::not_modified());
//! let response = fixture.get(&ApiRequest::get("/repos/o/r")).unwrap();
//! assert_eq!(response.status, 304);
//! fixture.assert_drained();
//! ```

use super::{ApiRequest, ApiResponse, Transport};
use crate::error::{Result, ScoutError};
use anyhow::{anyhow, bail};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Mutex;

/// What a scripted exchange expects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expect {
    /// A GET whose path equals this string exactly.
    Get(String),
    /// Any GET.
    AnyGet,
    /// Any GraphQL call.
    Graphql,
}

/// How a scripted exchange answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Response(ApiResponse),
    /// Fails as if the network were down (`ScoutError::NetworkUnavailable`).
    NetworkDown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exchange {
    pub expect: Expect,
    pub reply: Reply,
}

/// A request as the fixture saw it.
#[derive(Clone, Debug, PartialEq)]
pub enum Recorded {
    Get(ApiRequest),
    Graphql { query: String, variables: Value },
}

impl Recorded {
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Get(request) => Some(&request.path),
            Self::Graphql { .. } => None,
        }
    }
}

#[derive(Default)]
pub struct FixtureTransport {
    queue: Mutex<VecDeque<Exchange>>,
    log: Mutex<Vec<Recorded>>,
}

impl FixtureTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, exchange: Exchange) -> &Self {
        lock(&self.queue).push_back(exchange);
        self
    }

    pub fn expect_get(&self, path: impl Into<String>, response: ApiResponse) -> &Self {
        self.push(Exchange {
            expect: Expect::Get(path.into()),
            reply: Reply::Response(response),
        })
    }

    pub fn expect_any_get(&self, response: ApiResponse) -> &Self {
        self.push(Exchange {
            expect: Expect::AnyGet,
            reply: Reply::Response(response),
        })
    }

    pub fn expect_graphql(&self, response: ApiResponse) -> &Self {
        self.push(Exchange {
            expect: Expect::Graphql,
            reply: Reply::Response(response),
        })
    }

    /// The next GET to `path` fails with a network error.
    pub fn expect_network_down(&self, path: impl Into<String>) -> &Self {
        self.push(Exchange {
            expect: Expect::Get(path.into()),
            reply: Reply::NetworkDown,
        })
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<Recorded> {
        lock(&self.log).clone()
    }

    pub fn request_count(&self) -> usize {
        lock(&self.log).len()
    }

    /// Scripted exchanges not yet consumed.
    pub fn remaining(&self) -> usize {
        lock(&self.queue).len()
    }

    /// Panics if scripted exchanges were not consumed (test helper).
    pub fn assert_drained(&self) {
        let queue = lock(&self.queue);
        assert!(
            queue.is_empty(),
            "{} scripted exchange(s) not consumed: {:?}",
            queue.len(),
            queue.iter().map(|e| &e.expect).collect::<Vec<_>>()
        );
    }

    fn answer(&self, recorded: Recorded) -> Result<ApiResponse> {
        let description = match &recorded {
            Recorded::Get(request) => format!("GET {}", request.path),
            Recorded::Graphql { .. } => "GraphQL request".to_owned(),
        };
        let next = {
            lock(&self.log).push(recorded.clone());
            lock(&self.queue).pop_front()
        };
        let Some(exchange) = next else {
            bail!("fixture: unexpected {description}; nothing scripted");
        };
        let matches = match (&exchange.expect, &recorded) {
            (Expect::Get(path), Recorded::Get(request)) => *path == request.path,
            (Expect::AnyGet, Recorded::Get(_)) => true,
            (Expect::Graphql, Recorded::Graphql { .. }) => true,
            _ => false,
        };
        if !matches {
            return Err(anyhow!(
                "fixture: expected {:?}, got {description}",
                exchange.expect
            ));
        }
        match exchange.reply {
            Reply::Response(response) => Ok(response),
            Reply::NetworkDown => Err(ScoutError::NetworkUnavailable {
                message: format!("fixture network down for {description}"),
            }
            .into()),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Transport for FixtureTransport {
    fn name(&self) -> &'static str {
        "fixture"
    }

    fn get(&self, request: &ApiRequest) -> Result<ApiResponse> {
        self.answer(Recorded::Get(request.clone()))
    }

    fn graphql(&self, query: &str, variables: &Value) -> Result<ApiResponse> {
        self.answer(Recorded::Graphql {
            query: query.to_owned(),
            variables: variables.clone(),
        })
    }
}
