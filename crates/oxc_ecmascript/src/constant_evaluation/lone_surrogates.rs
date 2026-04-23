//! Detection helpers for bailing out of folds that would drop the
//! `lone_surrogates` flag.
//!
//! Some JS strings contain unpaired UTF-16 surrogates that are not
//! representable in UTF-8. `oxc_ast` stores them in a special encoding:
//! the string's bytes use `\u{FFFD}XXXX` (U+FFFD followed by four
//! lowercase hex digits) as an escape sequence — surrogate code points
//! as `\u{FFFD}d800..\u{FFFD}dfff`, and literal U+FFFD as
//! `\u{FFFD}fffd`. The `StringLiteral::lone_surrogates` /
//! `TemplateElement::lone_surrogates` flag tells codegen to decode the
//! escape back into `\uXXXX` sequences; when the flag is clear, codegen
//! emits the bytes as-is.
//!
//! The flag and the bytes are equally load-bearing. Two strings with
//! the same bytes but different flags are different strings at runtime.
//! Folds that produce a new `ConstantValue::String` discard the flag
//! (and the surrounding `value_to_expr` defaults the new literal's flag
//! to `false`), so any fold consuming a `lone_surrogates: true` input
//! would silently corrupt the value. The helpers here let callers
//! detect that and bail out rather than fold.
//!
//! The detection is conservative — callers over-approximate rather than
//! risk a missed bailout. A false positive only skips a fold that could
//! have been performed; it never produces incorrect output.

use oxc_ast::ast::*;
use oxc_syntax::operator::BinaryOperator;

use crate::GlobalContext;

use super::ConstantValue;

/// Returns `true` if `s` contains the lone-surrogate encoding pattern
/// `\u{FFFD}XXXX` where `XXXX` is a surrogate-range hex value
/// (`d800`..=`dfff`) or the self-escape `fffd`.
///
/// Scans the raw bytes without consulting any AST flag, so it also
/// matches a genuine U+FFFD that happens to precede four matching hex
/// characters. That false-positive case only causes callers to skip a
/// fold — it cannot produce wrong output.
pub fn str_has_lone_surrogate_encoding(s: &str) -> bool {
    let bytes = s.as_bytes();
    // U+FFFD is `EF BF BD` in UTF-8; bail early if the leading byte is
    // absent (the common case for ASCII-heavy code).
    if !bytes.contains(&0xEF) {
        return false;
    }
    // 3 bytes for U+FFFD + 4 bytes for the hex suffix.
    bytes.windows(7).any(|w| w[..3] == [0xEF, 0xBF, 0xBD] && is_lone_surrogate_suffix(&w[3..]))
}

fn is_lone_surrogate_suffix(b: &[u8]) -> bool {
    debug_assert_eq!(b.len(), 4);
    // Surrogate range d800–dfff, lowercase hex.
    (b[0] == b'd'
        && matches!(b[1], b'8'..=b'9' | b'a'..=b'f')
        && matches!(b[2], b'0'..=b'9' | b'a'..=b'f')
        && matches!(b[3], b'0'..=b'9' | b'a'..=b'f'))
        // Self-escape for a real U+FFFD inside a lone-surrogate string.
        || b == b"fffd"
}

/// Returns `true` if the expression, when stringified, may carry the
/// lone-surrogate encoding.
///
/// Used at fold sites that would otherwise consume the expression's
/// string value. When this returns `true`, the caller must skip the
/// fold — otherwise the fold would produce a new string literal with
/// `lone_surrogates: false`, silently corrupting the value.
///
/// Conservatively over-approximates: for kinds it can't analyse
/// precisely (e.g. an identifier whose initializer uses the encoding),
/// it returns `true`. False positives only cause a missed fold.
///
/// Identifiers are resolved through `ctx.get_constant_value_for_reference_id`
/// and the resulting `ConstantValue::String` bytes are byte-scanned. That
/// loses the AST flag but is sound for bail-out purposes: if the source
/// was a lone-surrogate literal, its bytes contain the encoding; if it
/// wasn't but the bytes happen to match, the false positive only skips a
/// fold.
pub fn expr_may_have_lone_surrogates<'a>(
    expr: &Expression<'a>,
    ctx: &impl GlobalContext<'a>,
) -> bool {
    match expr {
        Expression::StringLiteral(s) => s.lone_surrogates,
        Expression::TemplateLiteral(t) => {
            t.quasis.iter().any(|q| q.lone_surrogates)
                || t.expressions.iter().any(|e| expr_may_have_lone_surrogates(e, ctx))
        }
        Expression::BinaryExpression(e) if e.operator == BinaryOperator::Addition => {
            expr_may_have_lone_surrogates(&e.left, ctx)
                || expr_may_have_lone_surrogates(&e.right, ctx)
        }
        Expression::ArrayExpression(arr) => arr
            .elements
            .iter()
            .any(|el| el.as_expression().is_some_and(|e| expr_may_have_lone_surrogates(e, ctx))),
        Expression::Identifier(ident) => ident
            .reference_id
            .get()
            .and_then(|rid| ctx.get_constant_value_for_reference_id(rid))
            .is_some_and(|cv| match cv {
                ConstantValue::String(s) => str_has_lone_surrogate_encoding(&s),
                _ => false,
            }),
        Expression::UnaryExpression(e) => expr_may_have_lone_surrogates(&e.argument, ctx),
        Expression::LogicalExpression(e) => {
            expr_may_have_lone_surrogates(&e.left, ctx)
                || expr_may_have_lone_surrogates(&e.right, ctx)
        }
        Expression::ConditionalExpression(e) => {
            expr_may_have_lone_surrogates(&e.consequent, ctx)
                || expr_may_have_lone_surrogates(&e.alternate, ctx)
        }
        Expression::SequenceExpression(e) => {
            e.expressions.last().is_some_and(|e| expr_may_have_lone_surrogates(e, ctx))
        }
        Expression::ParenthesizedExpression(e) => expr_may_have_lone_surrogates(&e.expression, ctx),
        _ => false,
    }
}
