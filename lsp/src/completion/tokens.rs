use nom::{
    branch::alt,
    bytes::complete::{tag, take_while},
    character::complete::{alpha1, alphanumeric1, digit0, multispace0, none_of, one_of},
    combinator::{map, opt, recognize, value},
    multi::{many0, many0_count, separated_list0},
    sequence::{delimited, pair, preceded, separated_pair},
    IResult, Parser,
};

#[derive(Debug, PartialEq, Clone)]
/// Enum that indicates which completion strategy to use.
pub enum CompletionType {
    NonCompletionSymbol,
    OtherSymbol,
    Jet,
    Assignment(String),
    Identifier(String),
    ClosingType,
}

/// Parses single characters acting as triggers. As we define '<' and ':' as symbols for completion
/// inside server capabilities, we don't want to trigger completion every time these symbols are pressed.
fn parse_symbol(input: &str) -> IResult<&str, CompletionType> {
    let mut parser = alt((
        value(CompletionType::NonCompletionSymbol, one_of("<:")),
        value(CompletionType::OtherSymbol, none_of("<:")),
    ));

    parser.parse(input)
}

/// Parses the specific `jet::` namespace token, optionally followed by an identifier for the correct
/// completion trigger.
fn parse_jet(input: &str) -> IResult<&str, CompletionType> {
    let mut parser = value(
        CompletionType::Jet,
        recognize(pair(
            tag("jet::"),
            opt(take_while(|c: char| c.is_alphanumeric() || c == '_')),
        )),
    );
    parser.parse(input)
}

/// Parses standard identifiers consisting of alphanumeric characters and underscores.
fn parse_identifier(input: &str) -> IResult<&str, &str> {
    let mut parser = recognize(pair(
        alt((alpha1, tag("_"))),
        many0_count(alt((alphanumeric1, tag("_")))),
    ));
    parser.parse(input)
}

/// Parses a parenthesized, comma-separated list of patterns.
fn parse_tuple(input: &str) -> IResult<&str, &str> {
    let mut parser = recognize(delimited(
        tag("("),
        separated_list0(delimited(multispace0, tag(","), multispace0), parse_pattern),
        tag(")"),
    ));

    parser.parse(input)
}

/// Parses a bracketed, comma-separated list of patterns.
fn parse_array(input: &str) -> IResult<&str, &str> {
    recognize(delimited(
        tag("["),
        separated_list0(delimited(multispace0, tag(","), multispace0), parse_pattern),
        tag("]"),
    ))
    .parse(input)
}

/// Parses a structural pattern which can be a tuple, array, or identifier.
fn parse_pattern(input: &str) -> IResult<&str, &str> {
    recognize(alt((parse_tuple, parse_array, parse_identifier))).parse(input)
}

/// Parses an array type like `[type; number]`
fn parse_type_array(input: &str) -> IResult<&str, &str> {
    recognize(delimited(
        tag("["),
        separated_pair(
            parse_pattern,
            delimited(multispace0, tag(";"), multispace0),
            digit0,
        ),
        tag("]"),
    ))
    .parse(input)
}

/// Parses type for assignment. Almost identical to [`parse_pattern`], but handles arrays
/// differently
fn parse_ty(input: &str) -> IResult<&str, &str> {
    recognize(alt((parse_tuple, parse_type_array, parse_identifier))).parse(input)
}

/// Parses a variable assignment statement ending with a completion trigger '<'.
fn parse_assignment(input: &str) -> IResult<&str, CompletionType> {
    let mut parser = map(
        (
            preceded(multispace0, tag("let")),
            preceded(multispace0, parse_pattern),
            preceded(multispace0, tag(":")),
            preceded(multispace0, parse_ty),
            preceded(multispace0, tag("=")),
            preceded(multispace0, tag("<")),
            opt(parse_pattern),
        ),
        |(_, _, _, s, ..)| CompletionType::Assignment(s.to_string()),
    );

    parser.parse(input)
}

/// Parses the closure of a type cast `>::`, which triggers the 'into' keyword.
fn parse_type_end(input: &str) -> IResult<&str, CompletionType> {
    let mut parser = value(
        CompletionType::ClosingType,
        (
            preceded(multispace0, tag(">")),
            preceded(multispace0, tag("::")),
            opt(parse_identifier),
        ),
    );

    parser.parse(input)
}

/// Main entry point that parses a sequence of tokens and returns the last significant completion event.
pub fn parse(input: &str) -> Option<CompletionType> {
    let mut parser = many0(preceded(
        multispace0,
        alt((
            parse_assignment,
            parse_type_end,
            parse_jet,
            map(parse_identifier, |t| {
                CompletionType::Identifier(t.to_owned())
            }),
            parse_symbol,
        )),
    ));

    let output = parser.parse(input);
    let Ok(completion) = output else {
        return None;
    };

    completion.1.last().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_assignment() {
        let input = "let var: u32 = <";
        assert_eq!(
            parse(input),
            Some(CompletionType::Assignment("u32".to_string()))
        );
    }

    #[test]
    fn test_parse_assignment_nested_tuples() {
        let input = "let (a,b):  (u32, (u32, u32)) = <";
        assert_eq!(
            parse(input),
            Some(CompletionType::Assignment("(u32, (u32, u32))".to_string()))
        );
    }

    #[test]
    fn test_parse_assignment_nested_arrays() {
        let input = "let (a,b): [(u32, u32); 12] = <";
        assert_eq!(
            parse(input),
            Some(CompletionType::Assignment("[(u32, u32); 12]".to_string()))
        );
    }

    #[test]
    fn test_parse_jet() {
        let input = "other keywords jet::";
        assert_eq!(parse(input), Some(CompletionType::Jet));
    }

    #[test]
    fn test_parse_jet_with_identifier() {
        let input = "jet::add_32";
        assert_eq!(parse(input), Some(CompletionType::Jet));
    }

    #[test]
    fn test_parse_closing_type() {
        let input = ">::";
        assert_eq!(parse(input), Some(CompletionType::ClosingType));
    }

    #[test]
    fn test_parse_closing_type_with_identifier() {
        let input = ">::method";
        assert_eq!(parse(input), Some(CompletionType::ClosingType));
    }

    #[test]
    fn test_parse_non_completion_symbol_colon() {
        let input = ":";
        assert_eq!(parse(input), Some(CompletionType::NonCompletionSymbol));
    }

    #[test]
    fn test_parse_non_completion_symbol_angle() {
        let input = "<";
        assert_eq!(parse(input), Some(CompletionType::NonCompletionSymbol));
    }

    #[test]
    fn test_parse_other_symbol() {
        let input = "some other keywords";
        assert_eq!(
            parse(input),
            Some(CompletionType::Identifier("keywords".to_owned()))
        );
    }

    #[test]
    fn test_parse_mixed_sequence() {
        let input = "let x: T = < jet::";
        assert_eq!(parse(input), Some(CompletionType::Jet));
    }

    #[test]
    fn test_parse_empty() {
        let input = "";
        assert_eq!(parse(input), None);
    }
}
