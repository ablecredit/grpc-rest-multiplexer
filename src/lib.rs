#![feature(unboxed_closures)]

use axum::{
    http::Request,
    response::{IntoResponse, Response},
};
use futures::{future::BoxFuture, ready};
use http::{
    header::{HeaderName, ACCEPT, CONTENT_TYPE, HOST},
    request::Parts,
    HeaderValue, Method,
};

use std::{
    convert::Infallible,
    task::{Context, Poll},
};
use tonic::{service::interceptor::InterceptorLayer, Status};
use tower::{
    layer::util::{Identity, Stack},
    Service,
};
use tower_http::cors::{AllowOrigin, CorsLayer};

#[macro_use]
extern crate log;

pub struct MultiplexService<A, B> {
    rest: A,
    rest_ready: bool,
    grpc: B,
    grpc_ready: bool,
}

impl<A, B> MultiplexService<A, B> {
    pub fn new(rest: A, grpc: B) -> Self {
        Self {
            rest,
            rest_ready: false,
            grpc,
            grpc_ready: false,
        }
    }
}

impl<A, B> Clone for MultiplexService<A, B>
where
    A: Clone,
    B: Clone,
{
    fn clone(&self) -> Self {
        Self {
            rest: self.rest.clone(),
            grpc: self.grpc.clone(),
            // the cloned services probably wont be ready
            rest_ready: false,
            grpc_ready: false,
        }
    }
}

impl<A, B> Service<Request<hyper::body::Body>> for MultiplexService<A, B>
where
    A: Service<Request<hyper::body::Body>, Error = Infallible>,
    A::Response: IntoResponse,
    A::Future: Send + 'static,
    B: Service<Request<hyper::body::Body>>,
    B::Response: IntoResponse,
    B::Future: Send + 'static,
{
    type Response = Response;
    type Error = B::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // drive readiness for each inner service and record which is ready
        loop {
            match (self.rest_ready, self.grpc_ready) {
                (true, true) => {
                    return Ok(()).into();
                }
                (false, _) => {
                    ready!(self.rest.poll_ready(cx)).map_err(|err| match err {})?;
                    self.rest_ready = true;
                }
                (_, false) => {
                    ready!(self.grpc.poll_ready(cx))?;
                    self.grpc_ready = true;
                }
            }
        }
    }

    fn call(&mut self, req: Request<hyper::body::Body>) -> Self::Future {
        // require users to call `poll_ready` first, if they don't we're allowed to panic
        // as per the `tower::Service` contract
        assert!(
            self.grpc_ready,
            "grpc service not ready. Did you forget to call `poll_ready`?"
        );
        assert!(
            self.rest_ready,
            "rest service not ready. Did you forget to call `poll_ready`?"
        );

        // if we get a grpc request call the grpc service, otherwise call the rest service
        // when calling a service it becomes not-ready so we have drive readiness again
        if is_grpc_request(&req) {
            self.grpc_ready = false;
            let future = self.grpc.call(req);
            Box::pin(async move {
                let res = future.await?;
                Ok(res.into_response())
            })
        } else {
            self.rest_ready = false;
            let future = self.rest.call(req);
            Box::pin(async move {
                let res = future.await.map_err(|err| {
                    error!("Error during json await: {err:?}");
                    match err {}
                })?;
                Ok(res.into_response())
            })
        }
    }
}

fn cors_layer_allow_header() -> CorsLayer {
    CorsLayer::new().allow_headers([
        ACCEPT,
        HOST,
        CONTENT_TYPE,
        HeaderName::from_static("x-c"),
        HeaderName::from_static("x-u"),
        HeaderName::from_static("x-a"),
        HeaderName::from_static("x-o"),
        HeaderName::from_static("x-rsa"),
        HeaderName::from_static("x-auth"),
        HeaderName::from_static("x-ua"),
        HeaderName::from_static("x-host"),
        HeaderName::from_static("x-key"),
        HeaderName::from_static("x-app"),
        HeaderName::from_static("x-rqid"),
        HeaderName::from_static("x-provider"),
        HeaderName::from_static("x-grpc-web"),
        HeaderName::from_static("x-user-agent"),
    ])
}

fn is_grpc_request<B>(req: &Request<B>) -> bool {
    req.headers()
        .get("content-type")
        .map(|content_type| {
            info!(
                "{}",
                String::from_utf8(content_type.as_bytes().to_vec()).unwrap()
            );
            content_type.as_bytes()
        })
        .filter(|content_type| content_type.starts_with(b"application/grpc"))
        .is_some()
}

pub fn xai_rest_layer() -> Stack<CorsLayer, Identity> {
    tower::ServiceBuilder::new()
        .layer(
            cors_layer_allow_header()
                .allow_origin(AllowOrigin::predicate(|origin: &HeaderValue, _: &Parts| {
                    origin.is_empty()
                        || origin.as_bytes().ends_with(b"xambit.io")
                        || origin.as_bytes().starts_with(b"http://localhost")
                }))
                .allow_methods([
                    Method::POST,
                    Method::PUT,
                    Method::DELETE,
                    Method::GET,
                    Method::OPTIONS,
                ]),
        )
        .into_inner()
}

pub fn xai_grpc_layer<F>(extractor: F) -> Stack<InterceptorLayer<F>, Stack<CorsLayer, Identity>>
where
    F: FnMut(tonic::Request<()>) -> anyhow::Result<tonic::Request<()>, Status>,
{
    tower::ServiceBuilder::new()
        .layer(
            cors_layer_allow_header()
                .allow_origin(AllowOrigin::predicate(|origin: &HeaderValue, _: &Parts| {
                    origin.is_empty()
                        || origin.as_bytes().ends_with(b"xambit.io")
                        || origin.as_bytes().starts_with(b"http://localhost")
                }))
                .allow_methods([Method::POST]),
        )
        .layer(tonic::service::interceptor(extractor))
        .into_inner()
}
