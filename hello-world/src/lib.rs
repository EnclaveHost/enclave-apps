//! hello-world - the platform's smallest app, as a WASIp3 async component.
//!
//! This is the p3 counterpart of the p2 original: the export moved from
//! wasi:http/incoming-handler@0.2 (proxy world, ResponseOutparam plumbing)
//! to wasi:http/handler@0.3 (service world), where `handle` is an async
//! function and the response body is a native component-model stream. The
//! platform detects the contract from the binary at upload and routes the
//! version to p3-capable boxes; declare `{"wasi": "0.3"}` in the version
//! config so claims route before launch re-classification.
//!
//! Build (the blessed container; stable rustc ships no wasip3 std):
//!   docker run --rm -v "$PWD":/src enclave-wasip3-build
//! Local test:
//!   wasmtime serve -Scli -Shttp -Sp3 target/wasm32-wasip3/release/hello_world.wasm

use wasip3::http::types::{ErrorCode, Fields, Request, Response};
use wasip3::{spawn, wit_future, wit_stream};

struct Component;

impl wasip3::exports::http::handler::Guest for Component {
    async fn handle(_request: Request) -> Result<Response, ErrorCode> {
        let (mut body_tx, body_rx) = wit_stream::new();
        let (body_result_tx, body_result_rx) = wit_future::new(|| Ok(None));
        let (response, _transmit_result) =
            Response::new(Fields::new(), Some(body_rx), body_result_rx);
        // no trailers, and the transmit result is the host's business
        drop(body_result_tx);

        spawn(async move {
            let leftover = body_tx.write_all(b"Hello World!\n".to_vec()).await;
            debug_assert!(leftover.is_empty());
        });
        Ok(response)
    }
}

wasip3::http::service::export!(Component);
