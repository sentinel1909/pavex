use dep::Logger;
use pavex::blueprint::{router::GET, Blueprint};
use pavex::f;

pub fn handler(_logger: Logger) -> pavex::response::Response {
    todo!()
}

pub fn blueprint() -> Blueprint {
    let mut bp = Blueprint::new();
    dep::add_logger(&mut bp);
    bp.route(GET, "/home", f!(crate::handler));
    bp
}
