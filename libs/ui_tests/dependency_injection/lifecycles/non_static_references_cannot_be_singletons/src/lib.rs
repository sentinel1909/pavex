use pavex::blueprint::{router::GET, Blueprint};
use pavex::f;
use pavex::http::StatusCode;

#[derive(Clone)]
pub struct A;

#[derive(Clone)]
pub struct B;

pub fn a() -> A {
    todo!()
}

pub fn b(_a: &A) -> &B {
    todo!()
}

pub fn handler(_b: &B) -> StatusCode {
    todo!()
}

pub fn blueprint() -> Blueprint {
    let mut bp = Blueprint::new();
    bp.singleton(f!(crate::a));
    bp.singleton(f!(crate::b));
    bp.route(GET, "/", f!(crate::handler));
    bp
}
