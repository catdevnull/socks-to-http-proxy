use auth::Auth;
use base64::engine::general_purpose;
use base64::Engine;
use color_eyre::eyre::Result;

pub mod auth;

use hyper::header::{HeaderValue, PROXY_AUTHENTICATE};
use hyper::service::service_fn;
use tokio::net::TcpStream;
use tokio_socks::tcp::Socks5Stream;
use tracing::{debug, warn};

use std::net::SocketAddr;

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};

use hyper::upgrade::Upgraded;
use hyper::{Method, Request, Response};

use hyper_util::rt::TokioIo;

use hyper::client::conn::http1::Builder;
use hyper::server::conn::http1;

async fn proxy(
    req: Request<hyper::body::Incoming>,
    client_addr: SocketAddr,
    socks_addr: SocketAddr,
    auth: Option<&'static Auth>,
    allowed_domains: Option<&'static Vec<String>>,
    basic_http_header: Option<&'static HeaderValue>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>> {
    let mut authenticated = false;
    let hm = req.headers();

    if let Some(basic_http_header) = basic_http_header {
        let Some(http_auth) = hm.get("proxy-authorization") else {
            // When the request does not contain a Proxy-Authorization header,
            // send a 407 response code and a Proxy-Authenticate header
            let mut response = Response::new(full("Proxy Authentication Required"));
            *response.status_mut() = hyper::StatusCode::PROXY_AUTHENTICATION_REQUIRED;
            response.headers_mut().insert(
                PROXY_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"proxy\""),
            );
            return Ok(response);
        };
        if http_auth == basic_http_header {
            authenticated = true;
        }
    } else {
        authenticated = true;
    }

    if !authenticated {
        warn!("Failed auth attempt from: {}", client_addr);
        // http response code reference taken from tinyproxy
        let mut resp = Response::new(full("Unauthorized"));
        *resp.status_mut() = hyper::StatusCode::UNAUTHORIZED;
        return Ok(resp);
    }

    let uri = req.uri();
    let method = req.method();
    debug!("Proxying request: {} {}", method, uri);
    if let (Some(allowed_domains), Some(request_domain)) = (allowed_domains, req.uri().host()) {
        let domain = request_domain.to_owned();
        if !allowed_domains.contains(&domain) {
            warn!(
                "Access to domain {} is not allowed through the proxy.",
                domain
            );
            let mut resp = Response::new(full(
                "Access to this domain is not allowed through the proxy.",
            ));
            *resp.status_mut() = hyper::StatusCode::FORBIDDEN;
            return Ok(resp);
        }
    }

    if Method::CONNECT == req.method() {
        if let Some(addr) = host_addr(req.uri()) {
            tokio::task::spawn(async move {
                let hm = req.headers().clone();
                match hyper::upgrade::on(req).await {
                    Ok(upgraded) => {
                        if let Err(e) = tunnel(
                            upgraded,
                            addr,
                            socks_addr,
                            auth,
                            basic_http_header.clone(),
                            hm,
                        )
                        .await
                        {
                            warn!("server io error: {}", e);
                        };
                    }
                    Err(e) => warn!("upgrade error: {}", e),
                }
            });

            Ok(Response::new(empty()))
        } else {
            warn!("CONNECT host is not socket addr: {:?}", req.uri());
            let mut resp = Response::new(full("CONNECT must be to a socket address"));
            *resp.status_mut() = hyper::StatusCode::BAD_REQUEST;

            Ok(resp)
        }
    } else {
        let host = req.uri().host().expect("uri has no host");
        let port = req.uri().port_u16().unwrap_or(80);
        let addr = format!("{}:{}", host, port);

        let stream = match auth {
            Some(auth) => Socks5Stream::connect_with_password(
                socks_addr,
                addr,
                &auth.username,
                &auth.password,
            )
            .await
            .unwrap(),
            None => match (basic_http_header, hm.get("proxy-authorization")) {
                (None, Some(http_auth)) => {
                    // HACK: we pass through proxy auth header
                    let decoded_auth = String::from_utf8(
                        general_purpose::STANDARD.decode(
                            http_auth
                                .to_str()
                                .unwrap()
                                .trim_start_matches("Basic ")
                                .trim(),
                        )?,
                    )?;
                    let (username, password) = decoded_auth
                        .split_once(':')
                        .ok_or_else(|| color_eyre::eyre::eyre!("Invalid auth format"))?;
                    Socks5Stream::connect_with_password(socks_addr, addr, username, password)
                        .await
                        .unwrap()
                }
                _ => Socks5Stream::connect(socks_addr, addr).await?,
            },
        };
        let io = TokioIo::new(stream);

        let (mut sender, conn) = Builder::new()
            .preserve_header_case(true)
            .title_case_headers(true)
            .handshake(io)
            .await?;
        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                warn!("Connection failed: {:?}", err);
            }
        });

        let resp = sender.send_request(req).await?;
        Ok(resp.map(|b| b.boxed()))
    }
}

fn host_addr(uri: &hyper::Uri) -> Option<String> {
    uri.authority().map(|auth| auth.to_string())
}

fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

async fn connect_with_auth(
    socks_addr: SocketAddr,
    addr: String,
    auth: Option<Auth>,
    basic_http_header: Option<&'static HeaderValue>,
    hm: hyper::HeaderMap<hyper::header::HeaderValue>,
) -> Result<Socks5Stream<TcpStream>> {
    match auth {
        Some(auth) => Ok(Socks5Stream::connect_with_password(
            socks_addr,
            addr,
            &auth.username,
            &auth.password,
        )
        .await
        .unwrap()),
        None => match (basic_http_header, hm.get("proxy-authorization")) {
            (None, Some(http_auth)) => {
                println!("no http header configured but passed auth, trying to log into socks proxy with auth");
                let decoded_auth = String::from_utf8(
                    general_purpose::STANDARD.decode(
                        http_auth
                            .to_str()
                            .unwrap()
                            .trim_start_matches("Basic ")
                            .trim(),
                    )?,
                )?;
                let (username, password) = decoded_auth
                    .split_once(':')
                    .ok_or_else(|| color_eyre::eyre::eyre!("Invalid auth format"))?;
                println!("{} {} {}", username, password, addr);
                Ok(
                    Socks5Stream::connect_with_password(socks_addr, addr, username, password)
                        .await
                        .unwrap(),
                )
            }
            _ => Ok(Socks5Stream::connect(socks_addr, addr).await?),
        },
    }
}

async fn tunnel(
    upgraded: Upgraded,
    addr: String,
    socks_addr: SocketAddr,
    auth: Option<&Auth>,
    basic_http_header: Option<&'static HeaderValue>,
    hm: hyper::HeaderMap<hyper::header::HeaderValue>,
) -> Result<()> {
    let mut stream = connect_with_auth(
        socks_addr,
        addr,
        auth.map(|a| a.clone()),
        basic_http_header,
        hm,
    )
    .await?;

    let mut upgraded = TokioIo::new(upgraded);

    // Proxying data
    let (from_client, from_server) =
        tokio::io::copy_bidirectional(&mut upgraded, &mut stream).await?;

    // Print message when done
    debug!(
        "client wrote {} bytes and received {} bytes",
        from_client, from_server
    );
    Ok(())
}

pub async fn proxy_request(
    stream: TcpStream,
    client_addr: SocketAddr,
    socks_addr: SocketAddr,
    auth_details: Option<&'static Auth>,
    allowed_domains: Option<&'static Vec<String>>,
    basic_http_header: Option<&'static HeaderValue>,
) -> color_eyre::Result<()> {
    let io = TokioIo::new(stream);

    let serve_connection = service_fn(move |req| {
        proxy(
            req,
            client_addr,
            socks_addr,
            auth_details,
            allowed_domains,
            basic_http_header,
        )
    });

    tokio::task::spawn(async move {
        if let Err(err) = http1::Builder::new()
            .preserve_header_case(true)
            .title_case_headers(true)
            .serve_connection(io, serve_connection)
            .with_upgrades()
            .await
        {
            warn!("Failed to serve connection: {:?}", err);
        }
    });
    Ok(())
}
