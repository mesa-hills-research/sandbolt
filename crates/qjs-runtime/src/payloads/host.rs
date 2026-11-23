use crate::value::{ModId, Value};

payloads!(
    ModuleNamespaceData { module: Option<ModId> }, DomExceptionData { message: Value },
    PerformanceData {}, HostObjectData {},
);
