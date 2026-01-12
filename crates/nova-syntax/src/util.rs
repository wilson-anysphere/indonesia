use crate::ParseError;

pub(crate) fn sort_parse_errors(errors: &mut Vec<ParseError>) {
    errors.sort_by_key(|e| (e.range.start, e.range.end));
}
