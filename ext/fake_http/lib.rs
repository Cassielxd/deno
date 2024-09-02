// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use async_compression::tokio::write::BrotliEncoder;
use async_compression::tokio::write::GzipEncoder;
use async_compression::Level;
use base64::Engine;
use cache_control::CacheControl;
use deno_core::error::custom_error;
use deno_core::error::AnyError;
use deno_core::futures::future::pending;
use deno_core::futures::future::Pending;
use deno_core::futures::never::Never;
use deno_core::futures::{ready, SinkExt};
use deno_core::futures::stream::Peekable;
use deno_core::futures::FutureExt;
use deno_core::futures::StreamExt;
use deno_core::futures::TryFutureExt;
use deno_core::op2;
use deno_core::AsyncRefCell;
use deno_core::AsyncResult;
use deno_core::BufView;
use deno_core::ByteString;
use deno_core::CancelFuture;
use deno_core::CancelHandle;
use deno_core::CancelTryFuture;
use deno_core::JsBuffer;
use deno_core::OpState;
use deno_core::RcRef;
use deno_core::Resource;
use deno_core::ResourceId;
use deno_core::StringOrBuffer;
use flate2::write::GzEncoder;
use flate2::Compression;
use hyper_util::rt::TokioIo;
use hyper_v014::body::Bytes;
use hyper_v014::body::HttpBody;
use hyper_v014::body::SizeHint;
use hyper_v014::header::HeaderName;
use hyper_v014::header::HeaderValue;
use hyper_v014::service::Service;
use hyper_v014::Body;
use hyper_v014::HeaderMap;
use hyper_v014::Request;
use hyper_v014::Response;
use serde::Serialize;
use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::min;
use std::error::Error;
use std::future::Future;
use std::io;
use std::io::Write;
use std::mem::replace;
use std::mem::take;
use std::pin::Pin;
use std::rc::Rc;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;
use crate::reader_stream::ExternallyAbortableReaderStream;
use crate::reader_stream::ShutdownHandle;

pub mod compressible;
mod fly_accept_encoding;

mod reader_stream;


use fly_accept_encoding::Encoding;

pub type HttpSender = async_channel::Sender<RequestContext>;
pub type HttpReceiver = async_channel::Receiver<RequestContext>;

pub struct RequestContext {
    pub request: Request<Body>,
    pub response_tx: Sender<Response<Body>>,
}

deno_core::extension!(
  fake_http,
  deps = [deno_fetch,deno_web],
  ops = [
    op_fake_http_accept,
    op_fake_http_headers,
    op_fake_http_shutdown,
    op_fake_http_write_headers,
    op_fake_http_write_resource,
    op_fake_http_write,
  ],
    esm  = [ "01_fakehttp.js"],
    options = {
        recever:Option<HttpReceiver> ,
    },
    state = |state, options| {
        match options.recever{
            Some(r)=>{
                state.put(r);
            }
            None=>{

            }
        }

  },
);

pub enum HttpSocketAddr {
    IpSocket(std::net::SocketAddr),
    #[cfg(unix)]
    UnixSocket(tokio::net::unix::SocketAddr),
}

impl From<std::net::SocketAddr> for HttpSocketAddr {
    fn from(addr: std::net::SocketAddr) -> Self {
        Self::IpSocket(addr)
    }
}

#[cfg(unix)]
impl From<tokio::net::unix::SocketAddr> for HttpSocketAddr {
    fn from(addr: tokio::net::unix::SocketAddr) -> Self {
        Self::UnixSocket(addr)
    }
}


pub struct HttpStreamReadResource {
    pub rd: AsyncRefCell<HttpRequestReader>,
    cancel_handle: CancelHandle,
    size: SizeHint,
}

pub struct HttpStreamWriteResource {
    wr: AsyncRefCell<HttpResponseWriter>,
    accept_encoding: Encoding,
}

impl HttpStreamReadResource {
    fn new(request: Request<Body>) -> Self {
        let size = request.body().size_hint();
        Self {
            rd: HttpRequestReader::Headers(request).into(),
            size,
            cancel_handle: CancelHandle::new(),
        }
    }
}

impl Resource for HttpStreamReadResource {
    fn name(&self) -> Cow<str> {
        "httpReadStream".into()
    }

    fn read(self: Rc<Self>, limit: usize) -> AsyncResult<BufView> {
        Box::pin(async move {
            let mut rd = RcRef::map(&self, |r| &r.rd).borrow_mut().await;

            let body = loop {
                match &mut *rd {
                    HttpRequestReader::Headers(_) => {}
                    HttpRequestReader::Body(_, body) => break body,
                    HttpRequestReader::Closed => return Ok(BufView::empty()),
                }
                match take(&mut *rd) {
                    HttpRequestReader::Headers(request) => {
                        let (parts, body) = request.into_parts();
                        *rd = HttpRequestReader::Body(parts.headers, body.peekable());
                    }
                    _ => unreachable!(),
                };
            };

            let fut = async {
                let mut body = Pin::new(body);
                loop {
                    match body.as_mut().peek_mut().await {
                        Some(Ok(chunk)) if !chunk.is_empty() => {
                            let len = min(limit, chunk.len());
                            let buf = chunk.split_to(len);
                            let view = BufView::from(buf);
                            break Ok(view);
                        }
                        // This unwrap is safe because `peek_mut()` returned `Some`, and thus
                        // currently has a peeked value that can be synchronously returned
                        // from `next()`.
                        //
                        // The future returned from `next()` is always ready, so we can
                        // safely call `await` on it without creating a race condition.
                        Some(_) => match body.as_mut().next().await.unwrap() {
                            Ok(chunk) => assert!(chunk.is_empty()),
                            Err(err) => break Err(AnyError::from(err)),
                        },
                        None => break Ok(BufView::empty()),
                    }
                }
            };

            let cancel_handle = RcRef::map(&self, |r| &r.cancel_handle);
            fut.try_or_cancel(cancel_handle).await
        })
    }

    fn close(self: Rc<Self>) {
        self.cancel_handle.cancel();
    }

    fn size_hint(&self) -> (u64, Option<u64>) {
        (self.size.lower(), self.size.upper())
    }
}

impl HttpStreamWriteResource {
    fn new(
        response_tx: Sender<Response<Body>>,
        accept_encoding: Encoding,
    ) -> Self {
        Self {
            wr: HttpResponseWriter::Headers(response_tx).into(),
            accept_encoding,
        }
    }
}

impl Resource for HttpStreamWriteResource {
    fn name(&self) -> Cow<str> {
        "httpWriteStream".into()
    }
}

/// The read half of an HTTP stream.
pub enum HttpRequestReader {
    Headers(Request<Body>),
    Body(HeaderMap<HeaderValue>, Peekable<Body>),
    Closed,
}

impl Default for HttpRequestReader {
    fn default() -> Self {
        Self::Closed
    }
}

/// The write half of an HTTP stream.
enum HttpResponseWriter {
    Headers(Sender<Response<Body>>),
    Body {
        writer: Pin<Box<dyn tokio::io::AsyncWrite>>,
        shutdown_handle: ShutdownHandle,
    },
    BodyUncompressed(BodyUncompressedSender),
    Closed,
}

impl Default for HttpResponseWriter {
    fn default() -> Self {
        Self::Closed
    }
}

struct BodyUncompressedSender(Option<hyper_v014::body::Sender>);

impl BodyUncompressedSender {
    fn sender(&mut self) -> &mut hyper_v014::body::Sender {
        // This is safe because we only ever take the sender out of the option
        // inside of the shutdown method.
        self.0.as_mut().unwrap()
    }

    fn shutdown(mut self) {
        // take the sender out of self so that when self is dropped at the end of
        // this block, it doesn't get aborted
        self.0.take();
    }
}

impl From<hyper_v014::body::Sender> for BodyUncompressedSender {
    fn from(sender: hyper_v014::body::Sender) -> Self {
        BodyUncompressedSender(Some(sender))
    }
}

impl Drop for BodyUncompressedSender {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            sender.abort();
        }
    }
}

// We use a tuple instead of struct to avoid serialization overhead of the keys.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NextRequestResponse(
    // read_stream_rid:
    ResourceId,
    // write_stream_rid:
    ResourceId,
    // method:
    // This is a String rather than a ByteString because reqwest will only return
    // the method as a str which is guaranteed to be ASCII-only.
    String,
    // url:
    String,
);

#[op2(async)]
#[serde]
async fn op_fake_http_accept(
    state: Rc<RefCell<OpState>>,
    #[smi] rid: ResourceId,
) -> Result<Option<NextRequestResponse>, AnyError> {
    let receiver = { state.borrow().borrow::<HttpReceiver>().clone() };
    match receiver.recv().await {
        Ok(item) => {
            let RequestContext { request, response_tx, .. } = item;
            let (read_stream, write_stream, method, url) = build_http_stream_resource("http", request, response_tx);
            let read_stream_rid = state
                .borrow_mut()
                .resource_table
                .add_rc(Rc::new(read_stream));
            let write_stream_rid = state
                .borrow_mut()
                .resource_table
                .add_rc(Rc::new(write_stream));
            let r = NextRequestResponse(read_stream_rid, write_stream_rid, method, url);;
            Ok(Some(r))
        }
        Err(err) => Err(AnyError::from(err)),
    }
}
pub fn build_http_stream_resource(scheme: &'static str, request: Request<Body>, response_tx: mpsc::Sender<Response<Body>>) -> (HttpStreamReadResource, HttpStreamWriteResource, String, String) {
    let accept_encoding = {
        let encodings = fly_accept_encoding::encodings_iter_http_02(request.headers()).filter(|r| matches!(r, Ok((Some(Encoding::Brotli | Encoding::Gzip), _))));

        fly_accept_encoding::preferred(encodings).ok().flatten().unwrap_or(Encoding::Identity)
    };
    let method = request.method().to_string();
    let url = req_url(&request, scheme);
    let read_stream = HttpStreamReadResource::new(request);
    let write_stream =
        HttpStreamWriteResource::new(response_tx, accept_encoding);
    (read_stream, write_stream, method, url)
}

fn req_url(req: &Request<Body>, scheme: &'static str) -> String {
    let mut host = "127.0.0.1";
    if req.headers().get("host").is_some() {
        host = req.headers().get("host").unwrap().to_str().unwrap();
    }

    let path = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    [scheme, "://", &host, path].concat()
}

fn req_headers(
    header_map: &HeaderMap<HeaderValue>,
) -> Vec<(ByteString, ByteString)> {
    // We treat cookies specially, because we don't want them to get them
    // mangled by the `Headers` object in JS. What we do is take all cookie
    // headers and concat them into a single cookie header, separated by
    // semicolons.
    let cookie_sep = "; ".as_bytes();
    let mut cookies = vec![];

    let mut headers = Vec::with_capacity(header_map.len());
    for (name, value) in header_map.iter() {
        if name == hyper_v014::header::COOKIE {
            cookies.push(value.as_bytes());
        } else {
            let name: &[u8] = name.as_ref();
            let value = value.as_bytes();
            headers.push((name.into(), value.into()));
        }
    }

    if !cookies.is_empty() {
        headers.push(("cookie".into(), cookies.join(cookie_sep).into()));
    }

    headers
}

#[op2(async)]
async fn op_fake_http_write_headers(
    state: Rc<RefCell<OpState>>,
    #[smi] rid: u32,
    #[smi] status: u16,
    #[serde] headers: Vec<(ByteString, ByteString)>,
    #[serde] data: Option<StringOrBuffer>,
) -> Result<(), AnyError> {
    let stream = state
        .borrow_mut()
        .resource_table
        .get::<HttpStreamWriteResource>(rid)?;

    // Track supported encoding
    let encoding = stream.accept_encoding;

    let mut builder = Response::builder();
    // SAFETY: can not fail, since a fresh Builder is non-errored
    let hmap = unsafe { builder.headers_mut().unwrap_unchecked() };

    // Add headers
    hmap.reserve(headers.len() + 2);
    for (k, v) in headers.into_iter() {
        let v: Vec<u8> = v.into();
        hmap.append(
            HeaderName::try_from(k.as_slice())?,
            HeaderValue::try_from(v)?,
        );
    }
    ensure_vary_accept_encoding(hmap);

    let accepts_compression =
        matches!(encoding, Encoding::Brotli | Encoding::Gzip);
    let compressing = accepts_compression
        && (matches!(data, Some(ref data) if data.len() > 20) || data.is_none())
        && should_compress(hmap);

    if compressing {
        weaken_etag(hmap);
        // Drop 'content-length' header. Hyper will update it using compressed body.
        hmap.remove(hyper_v014::header::CONTENT_LENGTH);
        // Content-Encoding header
        hmap.insert(
            hyper_v014::header::CONTENT_ENCODING,
            HeaderValue::from_static(match encoding {
                Encoding::Brotli => "br",
                Encoding::Gzip => "gzip",
                _ => unreachable!(), // Forbidden by accepts_compression
            }),
        );
    }

    let (new_wr, body) = http_response(data, compressing, encoding)?;
    let body = builder.status(status).body(body)?;

    let mut old_wr = RcRef::map(&stream, |r| &r.wr).borrow_mut().await;
    let mut response_tx = match replace(&mut *old_wr, new_wr) {
        HttpResponseWriter::Headers(response_tx) => response_tx,
        _ => return Err(http_error("response headers already sent")),
    };

    match response_tx.send(body).await {
        Ok(_) => Ok(()),
        Err(_) => {
            Err(http_error("connection closed while sending response"))
        }
    }
}

#[op2]
#[serde]
fn op_fake_http_headers(
    state: &mut OpState,
    #[smi] rid: u32,
) -> Result<Vec<(ByteString, ByteString)>, AnyError> {
    let stream = state.resource_table.get::<HttpStreamReadResource>(rid)?;
    let rd = RcRef::map(&stream, |r| &r.rd)
        .try_borrow()
        .ok_or_else(|| http_error("already in use"))?;
    match &*rd {
        HttpRequestReader::Headers(request) => Ok(req_headers(request.headers())),
        HttpRequestReader::Body(headers, _) => Ok(req_headers(headers)),
        _ => unreachable!(),
    }
}

fn http_response(
    data: Option<StringOrBuffer>,
    compressing: bool,
    encoding: Encoding,
) -> Result<(HttpResponseWriter, hyper_v014::Body), AnyError> {
    const GZIP_DEFAULT_COMPRESSION_LEVEL: u8 = 1;
    match data {
        Some(data) if compressing => match encoding {
            Encoding::Brotli => {
                let mut writer = brotli::CompressorWriter::new(Vec::new(), 4096, 6, 22);
                writer.write_all(&data)?;
                Ok((HttpResponseWriter::Closed, writer.into_inner().into()))
            }
            Encoding::Gzip => {
                let mut writer = GzEncoder::new(
                    Vec::new(),
                    Compression::new(GZIP_DEFAULT_COMPRESSION_LEVEL.into()),
                );
                writer.write_all(&data)?;
                Ok((HttpResponseWriter::Closed, writer.finish()?.into()))
            }
            _ => unreachable!(), // forbidden by accepts_compression
        },
        Some(data) => {
            Ok((HttpResponseWriter::Closed, data.to_vec().into()))
        }
        None if compressing => {
            let (a, b) = tokio::io::duplex(64 * 1024);
            let (reader, _) = tokio::io::split(a);
            let (_, writer) = tokio::io::split(b);
            let writer: Pin<Box<dyn tokio::io::AsyncWrite>> = match encoding {
                Encoding::Brotli => {
                    Box::pin(BrotliEncoder::with_quality(writer, Level::Fastest))
                }
                Encoding::Gzip => Box::pin(GzipEncoder::with_quality(
                    writer,
                    Level::Precise(GZIP_DEFAULT_COMPRESSION_LEVEL.into()),
                )),
                _ => unreachable!(), // forbidden by accepts_compression
            };
            let (stream, shutdown_handle) =
                ExternallyAbortableReaderStream::new(reader);
            Ok((
                HttpResponseWriter::Body {
                    writer,
                    shutdown_handle,
                },
                Body::wrap_stream(stream),
            ))
        }
        None => {
            let (body_tx, body_rx) = Body::channel();
            Ok((
                HttpResponseWriter::BodyUncompressed(body_tx.into()),
                body_rx,
            ))
        }
    }
}

// If user provided a ETag header for uncompressed data, we need to
// ensure it is a Weak Etag header ("W/").
fn weaken_etag(hmap: &mut hyper_v014::HeaderMap) {
    if let Some(etag) = hmap.get_mut(hyper_v014::header::ETAG) {
        if !etag.as_bytes().starts_with(b"W/") {
            let mut v = Vec::with_capacity(etag.as_bytes().len() + 2);
            v.extend(b"W/");
            v.extend(etag.as_bytes());
            *etag = v.try_into().unwrap();
        }
    }
}

// Set Vary: Accept-Encoding header for direct body response.
// Note: we set the header irrespective of whether or not we compress the data
// to make sure cache services do not serve uncompressed data to clients that
// support compression.
fn ensure_vary_accept_encoding(hmap: &mut hyper_v014::HeaderMap) {
    if let Some(v) = hmap.get_mut(hyper_v014::header::VARY) {
        if let Ok(s) = v.to_str() {
            if !s.to_lowercase().contains("accept-encoding") {
                *v = format!("Accept-Encoding, {s}").try_into().unwrap()
            }
            return;
        }
    }
    hmap.insert(
        hyper_v014::header::VARY,
        HeaderValue::from_static("Accept-Encoding"),
    );
}

fn should_compress(headers: &hyper_v014::HeaderMap) -> bool {
    // skip compression if the cache-control header value is set to "no-transform" or not utf8
    fn cache_control_no_transform(
        headers: &hyper_v014::HeaderMap,
    ) -> Option<bool> {
        let v = headers.get(hyper_v014::header::CACHE_CONTROL)?;
        let s = match std::str::from_utf8(v.as_bytes()) {
            Ok(s) => s,
            Err(_) => return Some(true),
        };
        let c = CacheControl::from_value(s)?;
        Some(c.no_transform)
    }
    // we skip compression if the `content-range` header value is set, as it
    // indicates the contents of the body were negotiated based directly
    // with the user code and we can't compress the response
    let content_range = headers.contains_key(hyper_v014::header::CONTENT_RANGE);
    // assume body is already compressed if Content-Encoding header present, thus avoid recompressing
    let is_precompressed =
        headers.contains_key(hyper_v014::header::CONTENT_ENCODING);

    !content_range
        && !is_precompressed
        && !cache_control_no_transform(headers).unwrap_or_default()
        && headers
        .get(hyper_v014::header::CONTENT_TYPE)
        .map(compressible::is_content_compressible)
        .unwrap_or_default()
}

#[op2(async)]
async fn op_fake_http_write_resource(
    state: Rc<RefCell<OpState>>,
    #[smi] rid: ResourceId,
    #[smi] stream: ResourceId,
) -> Result<(), AnyError> {
    let http_stream = state
        .borrow()
        .resource_table
        .get::<HttpStreamWriteResource>(rid)?;
    let mut wr = RcRef::map(&http_stream, |r| &r.wr).borrow_mut().await;
    let resource = state.borrow().resource_table.get_any(stream)?;
    loop {
        match *wr {
            HttpResponseWriter::Headers(_) => {
                return Err(http_error("no response headers"))
            }
            HttpResponseWriter::Closed => {
                return Err(http_error("response already completed"))
            }
            _ => {}
        };

        let view = resource.clone().read(64 * 1024).await?; // 64KB
        if view.is_empty() {
            break;
        }

        match &mut *wr {
            HttpResponseWriter::Body { writer, .. } => {
                let mut result = writer.write_all(&view).await;
                if result.is_ok() {
                    result = writer.flush().await;
                }
                if let Err(err) = result {
                    assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
                    *wr = HttpResponseWriter::Closed;
                }
            }
            HttpResponseWriter::BodyUncompressed(body) => {
                let bytes = view.to_vec().into();
                if let Err(err) = body.sender().send_data(bytes).await {
                    assert!(err.is_closed());
                    *wr = HttpResponseWriter::Closed;
                }
            }
            _ => unreachable!(),
        };
    }
    Ok(())
}

#[op2(async)]
async fn op_fake_http_write(
    state: Rc<RefCell<OpState>>,
    #[smi] rid: ResourceId,
    #[buffer] buf: JsBuffer,
) -> Result<(), AnyError> {
    let stream = state
        .borrow()
        .resource_table
        .get::<HttpStreamWriteResource>(rid)?;
    let mut wr = RcRef::map(&stream, |r| &r.wr).borrow_mut().await;

    match &mut *wr {
        HttpResponseWriter::Headers(_) => Err(http_error("no response headers")),
        HttpResponseWriter::Closed => Err(http_error("response already completed")),
        HttpResponseWriter::Body { writer, .. } => {
            let mut result = writer.write_all(&buf).await;
            if result.is_ok() {
                result = writer.flush().await;
            }
            match result {
                Ok(_) => Ok(()),
                Err(err) => {
                    assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
                    *wr = HttpResponseWriter::Closed;
                    Err(http_error("response already completed"))
                }
            }
        }
        HttpResponseWriter::BodyUncompressed(body) => {
            let bytes = Bytes::from(buf.to_vec());
            match body.sender().send_data(bytes).await {
                Ok(_) => Ok(()),
                Err(err) => {
                    assert!(err.is_closed());
                    *wr = HttpResponseWriter::Closed;
                    Err(http_error("response already completed"))
                }
            }
        }
    }
}

/// Gracefully closes the write half of the HTTP stream. Note that this does not
/// remove the HTTP stream resource from the resource table; it still has to be
/// closed with `Deno.core.close()`.
#[op2(async)]
async fn op_fake_http_shutdown(
    state: Rc<RefCell<OpState>>,
    #[smi] rid: ResourceId,
) -> Result<(), AnyError> {
    let stream = state
        .borrow()
        .resource_table
        .get::<HttpStreamWriteResource>(rid)?;
    let mut wr = RcRef::map(&stream, |r| &r.wr).borrow_mut().await;
    let wr = take(&mut *wr);
    match wr {
        HttpResponseWriter::Body {
            mut writer,
            shutdown_handle,
        } => {
            shutdown_handle.shutdown();
            match writer.shutdown().await {
                Ok(_) => {}
                Err(err) => {
                    assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
                }
            }
        }
        HttpResponseWriter::BodyUncompressed(body) => {
            body.shutdown();
        }
        _ => {}
    };
    Ok(())
}

// Needed so hyper can use non Send futures
#[derive(Clone)]
struct LocalExecutor;

impl<Fut> hyper_v014::rt::Executor<Fut> for LocalExecutor
where
    Fut: Future + 'static,
    Fut::Output: 'static,
{
    fn execute(&self, fut: Fut) {
        deno_core::unsync::spawn(fut);
    }
}

impl<Fut> hyper::rt::Executor<Fut> for LocalExecutor
where
    Fut: Future + 'static,
    Fut::Output: 'static,
{
    fn execute(&self, fut: Fut) {
        deno_core::unsync::spawn(fut);
    }
}

fn http_error(message: &'static str) -> AnyError {
    custom_error("Http", message)
}

/// Filters out the ever-surprising 'shutdown ENOTCONN' errors.
fn filter_enotconn(
    result: Result<(), hyper_v014::Error>,
) -> Result<(), hyper_v014::Error> {
    if result
        .as_ref()
        .err()
        .and_then(|err| err.source())
        .and_then(|err| err.downcast_ref::<io::Error>())
        .filter(|err| err.kind() == io::ErrorKind::NotConnected)
        .is_some()
    {
        Ok(())
    } else {
        result
    }
}

/// Create a future that is forever pending.
fn never() -> Pending<Never> {
    pending()
}

trait CanDowncastUpgrade: Sized {
    fn downcast<T: AsyncRead + AsyncWrite + Unpin + 'static>(
        self,
    ) -> Result<(T, Bytes), Self>;
}

impl CanDowncastUpgrade for hyper::upgrade::Upgraded {
    fn downcast<T: AsyncRead + AsyncWrite + Unpin + 'static>(
        self,
    ) -> Result<(T, Bytes), Self> {
        let hyper::upgrade::Parts { io, read_buf, .. } =
            self.downcast::<TokioIo<T>>()?;
        Ok((io.into_inner(), read_buf))
    }
}

impl CanDowncastUpgrade for hyper_v014::upgrade::Upgraded {
    fn downcast<T: AsyncRead + AsyncWrite + Unpin + 'static>(
        self,
    ) -> Result<(T, Bytes), Self> {
        let hyper_v014::upgrade::Parts { io, read_buf, .. } = self.downcast()?;
        Ok((io, read_buf))
    }
}
