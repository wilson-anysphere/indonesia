use std::any::Any;
use std::borrow::Cow;

pub const NON_STRING_PANIC_PAYLOAD: &str = "<non-string panic payload>";

#[inline]
pub fn panic_payload_to_str<'a>(payload: &'a (dyn Any + Send)) -> Cow<'a, str> {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return Cow::Borrowed(message);
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return Cow::Borrowed(message.as_str());
    }
    Cow::Borrowed(NON_STRING_PANIC_PAYLOAD)
}

#[inline]
pub fn panic_payload_to_string(payload: &(dyn Any + Send)) -> Option<String> {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return Some((*message).to_string());
    }
    payload.downcast_ref::<String>().cloned()
}
