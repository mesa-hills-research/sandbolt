use crate::gc::{Trace, Tracer};
use crate::value::ObjId;

#[derive(Debug)]
pub struct ProxyData {
    pub target: Option<ObjId>,
    pub handler: Option<ObjId>,
    pub callable: bool,
}
impl Trace for ProxyData {
    fn trace(&self, tracer: &Tracer<'_>) {
        self.target.trace(tracer);
        self.handler.trace(tracer);
    }
}
