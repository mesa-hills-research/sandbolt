use crate::value::Value;

payloads!(
    NumberObjectData { value: Value },
    BooleanObjectData { value: Value },
    BigIntObjectData { value: Value },
    DateData { time: Value },
);
