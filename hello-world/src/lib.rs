use wasi::exports::http::incoming_handler::Guest;
use wasi::http::types::{
    Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam,
};

struct Component;

impl Guest for Component {
    fn handle(_request: IncomingRequest, response_out: ResponseOutparam) {
        let response = OutgoingResponse::new(Fields::new());
        response.set_status_code(200).unwrap();
        let body = response.body().unwrap();

        // Hand back the response head before streaming the body.
        ResponseOutparam::set(response_out, Ok(response));

        let out = body.write().unwrap();
        out.blocking_write_and_flush(b"Hello World!\n").unwrap();
        drop(out);
        OutgoingBody::finish(body, None).unwrap();
    }
}

wasi::http::proxy::export!(Component);
