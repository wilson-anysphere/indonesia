use std::any::Any;

pub(crate) fn panic_payload_to_string(payload: &(dyn Any + Send)) -> Option<String> {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return Some((*message).to_string());
    }
    payload.downcast_ref::<String>().cloned()
}
