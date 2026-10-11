use std::{
    collections::BTreeMap,
    future::Future,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use bytes::Bytes;
use http::{
    HeaderMap, HeaderValue, Method, Request, StatusCode, Uri,
    header::{
        AUTHORIZATION, COOKIE, LOCATION, PROXY_AUTHORIZATION, REFERER, USER_AGENT, WWW_AUTHENTICATE,
    },
};
use http_body_util::{BodyExt, Empty};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{
        Client,
        connect::{HttpConnector, dns::Name},
    },
    rt::{TokioExecutor, TokioIo},
};
use serde::{Deserialize, Serialize};
use tokio::time::{Instant as TokioInstant, timeout_at};
use tower_service::Service;
use url::Url;

const MAX_REDIRECTS: usize = 10;
const MAX_HTTP_RESPONSE_BYTES: usize = 64 * 1024;
const HTTP_BODY_CAPTURE_TIMEOUT: Duration = Duration::from_millis(500);
const USER_AGENT_VALUE: &str = concat!("wk-status-371tti-net/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct HttpTimings {
    pub dns_lookup_attempted: bool,
    pub dns_ms: Option<f64>,
    pub tcp_connect_ms: Option<f64>,
    pub tls_handshake_attempted: bool,
    pub tls_handshake_ms: Option<f64>,
    pub request_to_headers_ms: Option<f64>,
    pub total_ms: Option<f64>,
}

type ResolverFuture =
    Pin<Box<dyn Future<Output = Result<std::vec::IntoIter<SocketAddr>, std::io::Error>> + Send>>;
type TcpResponse = TokioIo<tokio::net::TcpStream>;
type TlsConnector = HttpsConnector<TimedTcpConnector>;
type ProbeBody = Empty<Bytes>;
type ProbeClient = Client<TimedTlsConnector, ProbeBody>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Dns,
    Tcp,
    Tls,
    Http,
}

#[derive(Default)]
struct TimingState {
    dns_lookup_attempted: bool,
    dns_ms: Option<f64>,
    tcp_ms: Option<f64>,
    tls_handshake_attempted: bool,
    tls_ms: Option<f64>,
    http_ms: Option<f64>,
    total_started: Option<Instant>,
    total_ms: Option<f64>,
    active: Option<(Phase, Instant)>,
}

#[derive(Clone, Default)]
pub(crate) struct PhaseRecorder {
    state: Arc<Mutex<TimingState>>,
}

impl PhaseRecorder {
    fn lock(&self) -> MutexGuard<'_, TimingState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn begin(&self, phase: Phase) {
        let mut state = self.lock();
        finish_active(&mut state);
        state.active = Some((phase, Instant::now()));
    }

    fn mark_attempted(&self, phase: Phase) {
        let mut state = self.lock();
        match phase {
            Phase::Dns => state.dns_lookup_attempted = true,
            Phase::Tls => state.tls_handshake_attempted = true,
            Phase::Tcp | Phase::Http => {}
        }
    }

    fn finish_if(&self, phase: Phase) {
        let mut state = self.lock();
        if state
            .active
            .as_ref()
            .is_some_and(|(active, _)| *active == phase)
        {
            finish_active(&mut state);
        }
    }

    fn finish_http(&self) {
        self.finish_if(Phase::Http);
    }

    fn start_total(&self) {
        let mut state = self.lock();
        state.total_started.get_or_insert_with(Instant::now);
    }

    fn finish_total(&self) {
        let mut state = self.lock();
        if state.total_ms.is_none() {
            state.total_ms = state
                .total_started
                .map(|started| started.elapsed().as_secs_f64() * 1000.0);
        }
    }

    fn reset(&self) {
        *self.lock() = TimingState::default();
    }

    pub(crate) fn finish(&self) -> HttpTimings {
        let mut state = self.lock();
        finish_active(&mut state);
        HttpTimings {
            dns_lookup_attempted: state.dns_lookup_attempted,
            dns_ms: state.dns_ms,
            tcp_connect_ms: state.tcp_ms,
            tls_handshake_attempted: state.tls_handshake_attempted,
            tls_handshake_ms: state.tls_ms,
            request_to_headers_ms: state.http_ms,
            total_ms: state.total_ms.or_else(|| {
                state
                    .total_started
                    .map(|started| started.elapsed().as_secs_f64() * 1000.0)
            }),
        }
    }
}

fn finish_active(state: &mut TimingState) {
    let Some((phase, started)) = state.active.take() else {
        return;
    };
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let value = match phase {
        Phase::Dns => &mut state.dns_ms,
        Phase::Tcp => &mut state.tcp_ms,
        Phase::Tls => &mut state.tls_ms,
        Phase::Http => &mut state.http_ms,
    };
    *value = Some(value.unwrap_or_default() + elapsed_ms);
}

#[derive(Clone)]
struct TimedResolver {
    timings: PhaseRecorder,
}

impl Service<Name> for TimedResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = std::io::Error;
    type Future = ResolverFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let timings = self.timings.clone();
        let host = name.as_str().to_owned();
        Box::pin(async move {
            timings.mark_attempted(Phase::Dns);
            timings.begin(Phase::Dns);
            let result = tokio::net::lookup_host((host.as_str(), 0)).await;
            timings.finish_if(Phase::Dns);
            match result {
                Ok(addresses) => {
                    timings.begin(Phase::Tcp);
                    Ok(addresses.collect::<Vec<_>>().into_iter())
                }
                Err(error) => Err(error),
            }
        })
    }
}

#[derive(Clone)]
struct TimedTcpConnector {
    inner: HttpConnector<TimedResolver>,
    timings: PhaseRecorder,
}

impl Service<Uri> for TimedTcpConnector {
    type Response = TcpResponse;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(std::io::Error::other)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let is_https = uri.scheme_str() == Some("https");
        if is_https {
            self.timings.mark_attempted(Phase::Tls);
        }
        if uri
            .host()
            .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
            .and_then(|host| host.parse::<IpAddr>().ok())
            .is_some()
        {
            self.timings.begin(Phase::Tcp);
        }
        let timings = self.timings.clone();
        let future = self.inner.call(uri);
        Box::pin(async move {
            let result = future.await.map_err(std::io::Error::other);
            timings.finish_if(Phase::Tcp);
            if result.is_ok() {
                timings.begin(if is_https { Phase::Tls } else { Phase::Http });
            }
            result
        })
    }
}

#[derive(Clone)]
struct TimedTlsConnector {
    inner: TlsConnector,
    timings: PhaseRecorder,
}

impl Service<Uri> for TimedTlsConnector {
    type Response = <TlsConnector as Service<Uri>>::Response;
    type Error = <TlsConnector as Service<Uri>>::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let is_https = uri.scheme_str() == Some("https");
        let timings = self.timings.clone();
        let future = self.inner.call(uri);
        Box::pin(async move {
            let result = future.await;
            if is_https {
                timings.finish_if(Phase::Tls);
                if result.is_ok() {
                    timings.begin(Phase::Http);
                }
            }
            result
        })
    }
}

pub(crate) struct HttpProbeClient {
    client: ProbeClient,
    timings: PhaseRecorder,
}

pub(crate) struct HttpProbeResponse {
    pub(crate) status_code: u16,
    pub(crate) body: Vec<u8>,
}

impl HttpProbeClient {
    pub(crate) fn reset_timings(&self) {
        self.timings.reset();
    }

    pub(crate) fn finish_timings(&self) -> HttpTimings {
        self.timings.finish()
    }

    pub(crate) async fn send(
        &self,
        raw_url: &str,
        method: Method,
        configured_headers: &BTreeMap<String, String>,
        timeout_duration: Duration,
    ) -> Result<HttpProbeResponse, String> {
        let request_deadline = TokioInstant::now() + timeout_duration;
        let mut current_url = Url::parse(raw_url).map_err(|_| "invalid HTTP URL".to_owned())?;
        current_url.set_fragment(None);

        let mut headers = HeaderMap::new();
        for (name, value) in configured_headers {
            let name = name
                .parse::<http::header::HeaderName>()
                .map_err(|error| format!("invalid HTTP header name: {error}"))?;
            let value = HeaderValue::from_str(value)
                .map_err(|error| format!("invalid HTTP header value: {error}"))?;
            headers.insert(name, value);
        }
        headers
            .entry(USER_AGENT)
            .or_insert(HeaderValue::from_static(USER_AGENT_VALUE));

        let mut redirects_followed = 0usize;

        loop {
            let uri = current_url
                .as_str()
                .parse::<Uri>()
                .map_err(|_| "invalid HTTP URL".to_owned())?;
            let mut request = Request::builder()
                .method(method.clone())
                .uri(uri)
                .body(Empty::<Bytes>::new())
                .map_err(|error| error.to_string())?;
            *request.headers_mut() = headers.clone();

            self.timings.start_total();
            let response = timeout_at(request_deadline, self.client.request(request))
                .await
                .map_err(|_| {
                    format!(
                        "HTTP request timed out after {}s",
                        timeout_duration.as_secs()
                    )
                })?
                .map_err(|error| error.to_string());
            self.timings.finish_http();
            let response = response?;
            let status = response.status();

            if !is_redirect(status) {
                self.timings.finish_total();
                let body = capture_response_body(response.into_body()).await;
                return Ok(HttpProbeResponse {
                    status_code: status.as_u16(),
                    body,
                });
            }
            let Some(location) = response.headers().get(LOCATION) else {
                self.timings.finish_total();
                let body = capture_response_body(response.into_body()).await;
                return Ok(HttpProbeResponse {
                    status_code: status.as_u16(),
                    body,
                });
            };
            if redirects_followed >= MAX_REDIRECTS {
                return Err("too many HTTP redirects".to_owned());
            }

            let location = location
                .to_str()
                .map_err(|_| "invalid HTTP redirect location".to_owned())?;
            let mut next_url = current_url
                .join(location)
                .map_err(|_| "invalid HTTP redirect location".to_owned())?;
            if !matches!(next_url.scheme(), "http" | "https") {
                return Err("HTTP redirect used an unsupported scheme".to_owned());
            }
            next_url.set_fragment(None);
            remove_sensitive_headers(&mut headers, &current_url, &next_url);
            if !(current_url.scheme() == "https" && next_url.scheme() == "http") {
                let mut referer = current_url.clone();
                let _ = referer.set_username("");
                let _ = referer.set_password(None);
                referer.set_fragment(None);
                if let Ok(value) = HeaderValue::from_str(referer.as_str()) {
                    headers.insert(REFERER, value);
                }
            }

            current_url = next_url;
            redirects_followed += 1;
            drop(response);
        }
    }
}

async fn capture_response_body(mut body: hyper::body::Incoming) -> Vec<u8> {
    let mut captured = Vec::new();
    let body_deadline = TokioInstant::now() + HTTP_BODY_CAPTURE_TIMEOUT;
    loop {
        let frame = match timeout_at(body_deadline, body.frame()).await {
            Ok(Some(Ok(frame))) => frame,
            Ok(Some(Err(_))) | Ok(None) | Err(_) => break,
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        let remaining = MAX_HTTP_RESPONSE_BYTES.saturating_sub(captured.len());
        if data.len() > remaining {
            captured.extend_from_slice(&data[..remaining]);
            break;
        }
        captured.extend_from_slice(&data);
        if captured.len() == MAX_HTTP_RESPONSE_BYTES {
            break;
        }
    }
    captured
}

pub(crate) fn build_client() -> Result<HttpProbeClient, String> {
    let timings = PhaseRecorder::default();
    let client = build_hyper_client(timings.clone())?;
    Ok(HttpProbeClient { client, timings })
}

fn build_hyper_client(timings: PhaseRecorder) -> Result<ProbeClient, String> {
    let resolver = TimedResolver {
        timings: timings.clone(),
    };
    let mut tcp = HttpConnector::new_with_resolver(resolver);
    tcp.enforce_http(false);
    tcp.set_nodelay(true);
    let tcp = TimedTcpConnector {
        inner: tcp,
        timings: timings.clone(),
    };
    let tls = HttpsConnectorBuilder::new()
        .try_with_platform_verifier()
        .map_err(|error| format!("failed to initialize TLS verifier: {error}"))?
        .https_or_http()
        .enable_http1()
        .wrap_connector(tcp);
    let connector = TimedTlsConnector {
        inner: tls,
        timings,
    };

    let mut builder = Client::builder(TokioExecutor::new());
    builder.pool_max_idle_per_host(0);
    builder.retry_canceled_requests(false);
    Ok(builder.build(connector))
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
}

fn remove_sensitive_headers(headers: &mut HeaderMap, previous: &Url, next: &Url) {
    let changed_origin = previous.host_str() != next.host_str()
        || previous.port_or_known_default() != next.port_or_known_default()
        || previous.scheme() != next.scheme();
    if changed_origin {
        headers.remove(AUTHORIZATION);
        headers.remove(COOKIE);
        headers.remove("cookie2");
        headers.remove(PROXY_AUTHORIZATION);
        headers.remove(WWW_AUTHENTICATE);
    }
}
