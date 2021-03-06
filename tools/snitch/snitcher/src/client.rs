// Copyright (c) Microsoft. All rights reserved.

use std::collections::HashMap;
use std::str;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::future::{self, Either};
use futures::{Future, Stream};
use http::Uri;
use hyper::header::{HeaderValue, CONTENT_LENGTH, CONTENT_TYPE, IF_MATCH};
use hyper::service::Service;
use hyper::{Body, Error as HyperError, Method, Request};
use log::{debug, error};
use serde::{de::DeserializeOwned, Serialize};
use serde_json;
use url::{form_urlencoded::Serializer as UrlSerializer, Url};

use crate::error::Error;

pub struct Client<S>
where
    S: 'static + Service<ReqBody = Body, ResBody = Body, Error = HyperError> + Send,
{
    service: Arc<Mutex<S>>,
    host_name: Url,
}

impl<S> Client<S>
where
    S: 'static + Service<ReqBody = Body, ResBody = Body, Error = HyperError> + Send,
    <S as Service>::Future: Send,
{
    pub fn new(service: S, host_name: Url) -> Client<S> {
        Client {
            service: Arc::new(Mutex::new(service)),
            host_name,
        }
    }

    pub fn request_bytes<BodyT>(
        &self,
        method: Method,
        path: &str,
        query: Option<HashMap<&str, &str>>,
        body: Option<BodyT>,
        add_if_match: bool,
    ) -> impl Future<Item = Option<Bytes>, Error = Error> + Send
    where
        BodyT: Serialize,
    {
        let query = query
            .and_then(|query| {
                let query = query
                    .iter()
                    .fold(&mut UrlSerializer::new(String::new()), |ser, (key, val)| {
                        ser.append_pair(key, val)
                    })
                    .finish();

                if !query.is_empty() {
                    Some(format!("?{}", query))
                } else {
                    None
                }
            })
            .unwrap_or_else(String::new);

        let url_copy = self.host_name.clone();
        let path_copy = path.to_owned();

        self.host_name
            // build the full url
            .join(&format!("{}?{}", path, query))
            .map_err(Error::from)
            .and_then(|url| {
                debug!("Making HTTP request with URL: {}", url);

                // NOTE: 'expect' here should be OK, because this is a type
                // conversion from url::Url to hyper::Uri and not really a URL
                // parse operation. At this point the URL has already been parsed
                // and is known to be good.
                let mut builder = Request::builder();
                let req = builder.method(method).uri(
                    url.as_str()
                        .parse::<Uri>()
                        .expect("Unexpected Url to Uri conversion failure"),
                );

                // add an `If-Match: "*"` header if we've been asked to
                if add_if_match {
                    req.header(IF_MATCH, HeaderValue::from_static("Any"));
                }

                // add request body if there is any
                if let Some(body) = body {
                    let serialized = serde_json::to_string(&body)?;
                    req.header(CONTENT_TYPE, "text/json");
                    req.header(CONTENT_LENGTH, format!("{}", serialized.len()).as_str());

                    Ok(req.body(Body::from(serialized))?)
                } else {
                    Ok(req.body(Body::empty())?)
                }
            })
            .map(move |req| {
                let uri = req.uri().clone();
                let res = self
                    .service
                    .lock()
                    .unwrap()
                    .call(req)
                    .map_err(move |err| {
                        error!("HTTP request to {:?} failed with {:?}", uri, err);
                        Error::from(err)
                    })
                    .and_then(|resp| {
                        let status = resp.status();
                        debug!("HTTP request succeeded with status {}", status);

                        let (_, body) = resp.into_parts();
                        body.concat2()
                            .and_then(move |body| Ok((status, body)))
                            .map_err(|err| {
                                error!("Reading response body, failed with {:?}", err);
                                Error::from(err)
                            })
                    })
                    .and_then(move |(status, body)| {
                        if status.is_success() {
                            if body.len() == 0 {
                                Ok(None)
                            } else {
                                Ok(Some(body.into_bytes()))
                            }
                        } else {
                            error!("HTTP request error: {}{}", url_copy, path_copy);
                            Err(Error::from((status, &*body)))
                        }
                    });

                Either::A(res)
            })
            .unwrap_or_else(|e| Either::B(future::err(e)))
    }

    pub fn request<BodyT, ResponseT>(
        &self,
        method: Method,
        path: &str,
        query: Option<HashMap<&str, &str>>,
        body: Option<BodyT>,
        add_if_match: bool,
    ) -> impl Future<Item = Option<ResponseT>, Error = Error> + Send
    where
        BodyT: Serialize,
        ResponseT: 'static + DeserializeOwned + Send,
    {
        self.request_bytes(method, path, query, body, add_if_match)
            .and_then(|bytes| {
                bytes
                    .map(|bytes| {
                        debug!("Request from bytes: {}", String::from_utf8_lossy(&bytes));

                        serde_json::from_slice::<ResponseT>(&bytes)
                            .map_err(Error::from)
                            .map(|resp| future::ok(Some(resp)))
                            .unwrap_or_else(future::err)
                    })
                    .unwrap_or_else(|| future::ok(None))
            })
    }

    pub fn request_str<BodyT>(
        &self,
        method: Method,
        path: &str,
        query: Option<HashMap<&str, &str>>,
        body: Option<BodyT>,
        add_if_match: bool,
    ) -> impl Future<Item = Option<String>, Error = Error> + Send
    where
        BodyT: Serialize,
    {
        self.request_bytes(method, path, query, body, add_if_match)
            .and_then(|bytes| {
                bytes
                    .map(|bytes| {
                        str::from_utf8(&bytes)
                            .map_err(Error::from)
                            .map(|s| future::ok(Some(s.to_owned())))
                            .unwrap_or_else(future::err)
                    })
                    .unwrap_or_else(|| future::ok(None))
            })
    }
}

impl<S> Clone for Client<S>
where
    S: 'static + Service<ReqBody = Body, ResBody = Body, Error = HyperError> + Send,
{
    fn clone(&self) -> Self {
        Client {
            service: self.service.clone(),
            host_name: self.host_name.clone(),
        }
    }
}
