/// Preserve existing callback strings so previously sent keyboards still work.
#[derive(Clone, Copy)]
pub(crate) enum ControlAction {
    Start,
    Stop,
}

impl ControlAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
        }
    }
}

pub(crate) enum Callback<'a> {
    Main,
    Detail(&'a str),
    Stream { id: &'a str, page: usize },
    Control { action: ControlAction, id: &'a str },
    Delete(&'a str),
}

impl<'a> Callback<'a> {
    pub(crate) fn parse(data: &'a str) -> Option<Self> {
        if data == "refresh_main" {
            return Some(Self::Main);
        }
        if let Some(id) = data.strip_prefix("view_").filter(|s| valid_id(s)) {
            return Some(Self::Detail(id));
        }
        if let Some(rest) = data.strip_prefix("stream_") {
            let (id, page) = rest.rsplit_once('_')?;
            if valid_id(id) {
                return Some(Self::Stream { id, page: page.parse().ok()? });
            }
        }
        if let Some(rest) = data.strip_prefix("op_") {
            let (action, id) = rest.split_once('_')?;
            let action = match action {
                "start" => ControlAction::Start,
                "stop" => ControlAction::Stop,
                _ => return None,
            };
            if valid_id(id) {
                return Some(Self::Control { action, id });
            }
        }
        data.strip_prefix("conf_del_").filter(|s| valid_id(s)).map(Self::Delete)
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_identifiers_containing_underscores() {
        assert!(matches!(Callback::parse("stream_room_a_12"), Some(Callback::Stream { id: "room_a", page: 12 })));
        assert!(matches!(Callback::parse("op_stop_room_a"), Some(Callback::Control { action: ControlAction::Stop, id: "room_a" })));
        assert!(matches!(Callback::parse("conf_del_room_a"), Some(Callback::Delete("room_a"))));
    }

    #[test]
    fn rejects_invalid_actions_pages_and_path_injection() {
        for input in ["op_delete_id", "stream_id_bad", "view_", "view_../config", "op_start_a/b", "unknown"] {
            assert!(Callback::parse(input).is_none());
        }
    }
}
