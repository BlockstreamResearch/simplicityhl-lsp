use nom::branch::alt;
use nom::bytes::complete::{tag, take_while};
use nom::character::complete::{alpha1, alphanumeric1, digit0, multispace0, none_of, one_of};
use nom::combinator::{map, opt, recognize, value};
use nom::multi::{many0, many0_count, separated_list0};
use nom::sequence::{delimited, pair, preceded, separated_pair};
use nom::{IResult, Parser};

/// Enum that indicates which completion strategy to use.
#[derive(Debug, PartialEq, Clone)]
pub enum CompletionType {
    NonCompletionSymbol,
    OtherSymbol,
    Jet,
    Assignment(String),
    Identifier(String),
    ClosingType,
}

/// Main entry point that parses a sequence of tokens and returns the last significant completion event.
pub fn parse(input: &str) -> Option<CompletionType> {
    let mut parser = many0(preceded(
        multispace0,
        alt((
            parse_assignment,
            parse_type_end,
            parse_jet,
            parse_word,
            parse_symbol,
        )),
    ));

    let output = parser.parse(input);
    let Ok(completion) = output else {
        return None;
    };

    completion.1.last().cloned()
}

/// Parses a variable assignment statement ending with a completion trigger '<'.
fn parse_assignment(input: &str) -> IResult<&str, CompletionType> {
    map(
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
    )
    .parse(input)
}

/// Parses the closure of a type cast `>::`, which triggers the 'into' keyword.
fn parse_type_end(input: &str) -> IResult<&str, CompletionType> {
    value(
        CompletionType::ClosingType,
        (
            preceded(multispace0, tag(">")),
            preceded(multispace0, tag("::")),
            opt(parse_identifier),
        ),
    )
    .parse(input)
}

/// Parses the specific `jet::` namespace token, optionally followed by an identifier for the correct
/// completion trigger.
fn parse_jet(input: &str) -> IResult<&str, CompletionType> {
    value(
        CompletionType::Jet,
        recognize(pair(
            tag("jet::"),
            opt(take_while(|c: char| c.is_alphanumeric() || c == '_')),
        )),
    )
    .parse(input)
}

/// Parses any other identifier.
fn parse_word(input: &str) -> IResult<&str, CompletionType> {
    map(parse_identifier, |t| {
        CompletionType::Identifier(t.to_owned())
    })
    .parse(input)
}

/// Parses single characters acting as triggers. As we define '<' and ':' as symbols for completion
/// inside server capabilities, we don't want to trigger completion every time these symbols are pressed.
fn parse_symbol(input: &str) -> IResult<&str, CompletionType> {
    alt((
        value(CompletionType::NonCompletionSymbol, one_of("<:")),
        value(CompletionType::OtherSymbol, none_of("<:")),
    ))
    .parse(input)
}

/// Parses a structural pattern which can be a tuple, array, or identifier.
fn parse_pattern(input: &str) -> IResult<&str, &str> {
    recognize(alt((parse_tuple, parse_array, parse_identifier))).parse(input)
}

/// Parses type for assignment. Almost identical to [`parse_pattern`], but handles arrays differently
fn parse_ty(input: &str) -> IResult<&str, &str> {
    recognize(alt((parse_tuple, parse_type_array, parse_identifier))).parse(input)
}

/// Parses a parenthesized, comma-separated list of patterns.
fn parse_tuple(input: &str) -> IResult<&str, &str> {
    recognize(delimited(
        tag("("),
        separated_list0(delimited(multispace0, tag(","), multispace0), parse_pattern),
        tag(")"),
    ))
    .parse(input)
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

/// Parses standard identifiers consisting of alphanumeric characters and underscores.
fn parse_identifier(input: &str) -> IResult<&str, &str> {
    recognize(pair(
        alt((alpha1, tag("_"))),
        many0_count(alt((alphanumeric1, tag("_")))),
    ))
    .parse(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_context_is_the_last_event_on_the_line() {
        use CompletionType::{
            Assignment, ClosingType, Identifier, Jet, NonCompletionSymbol, OtherSymbol,
        };

        for (input, expected) in [
            ("let var: u32 = <", Some(Assignment("u32".into()))),
            ("let ok: bool = <", Some(Assignment("bool".into()))),
            (
                "let (a,b):  (u32, (u32, u32)) = <",
                Some(Assignment("(u32, (u32, u32))".into())),
            ),
            (
                "let (a,b): [(u32, u32); 12] = <",
                Some(Assignment("[(u32, u32); 12]".into())),
            ),
            ("other keywords jet::", Some(Jet)),
            ("jet::add_32", Some(Jet)),
            ("let x: T = < jet::", Some(Jet)),
            (">::", Some(ClosingType)),
            (">::method", Some(ClosingType)),
            (":", Some(NonCompletionSymbol)),
            ("<", Some(NonCompletionSymbol)),
            ("let x:", Some(NonCompletionSymbol)),
            ("let ok: bool = a <", Some(NonCompletionSymbol)),
            ("some other keywords", Some(Identifier("keywords".into()))),
            ("a =", Some(OtherSymbol)),
            ("", None),
        ] {
            assert_eq!(parse(input), expected, "input: {input:?}");
        }
    }
}
