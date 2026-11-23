use crate::value::ObjId;

payloads!(
    ArrayBufferData {}, SharedArrayBufferData {},
    TypedArrayData { buffer: Option<ObjId> }, DataViewData { buffer: Option<ObjId> },
);
