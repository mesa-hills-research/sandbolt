use crate::value::Value;

payloads!(
    StringObjectData { value: Value },
    RegExpData { source: Value },
    StringIteratorData { target: Value },
    RegExpStringIteratorData { target: Value },
);
