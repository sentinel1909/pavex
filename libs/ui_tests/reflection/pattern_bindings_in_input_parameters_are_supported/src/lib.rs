use pavex::blueprint::{router::GET, Blueprint};
use pavex::f;

#[derive(Clone)]
pub struct Streamer {
    pub a: usize,
    pub b: isize,
}

pub fn streamer() -> Streamer {
    todo!()
}

pub fn stream_file(Streamer { a: _a, b: _b }: &Streamer) -> pavex::response::Response {
    todo!()
}

pub fn blueprint() -> Blueprint {
    let mut bp = Blueprint::new();
    bp.singleton(f!(crate::streamer));
    bp.route(GET, "/home", f!(crate::stream_file));
    bp
}
