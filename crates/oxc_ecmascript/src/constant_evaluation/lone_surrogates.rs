//! Detection helpers for bailing out of folds that would drop the `lone_surrogates` flag.
//!
//! `oxc_ast` stores strings with unpaired UTF-16 surrogates in a special encoding: each surrogate
//! code point is spelled `\u{FFFD}XXXX` (U+FFFD followed by four lowercase hex digits), with
//! `\u{FFFD}fffd` reserved as the self-escape for a real U+FFFD. The `StringLiteral` /
//! `TemplateElement` `lone_surrogates` flag tells codegen to decode those escapes back into
//! `\uXXXX`; when the flag is clear, codegen emits the bytes as-is.
//!
//! Two strings with the same bytes but different flags are different runtime values. Folds that
//! produce a new `ConstantValue::String` drop the flag, and `value_to_expr` then builds a literal
//! defaulting to `lone_surrogates: false` — so any fold consuming a `lone_surrogates: true` input
//! would silently corrupt the value. The helpers here let callers detect that and bail.
//!
//! Detection is conservative: false positives only skip a fold that could have been performed,
//! never produce wrong output.

use oxc_ast::ast::*;
use oxc_syntax::operator::BinaryOperator;

use crate::GlobalContext;

use super::ConstantValue;

/// Returns `true` if `s` contains the lone-surrogate encoding pattern `\u{FFFD}XXXX` — surrogate
/// range `d800`..=`dfff`, or the self-escape `fffd`.
///
/// Scans raw bytes without consulting any AST flag, so a genuine U+FFFD followed by four matching
/// hex characters also matches. That false positive only skips a fold.
pub fn str_has_lone_surrogate_encoding(s: &str) -> bool {
    let bytes = s.as_bytes();
    // U+FFFD is `EF BF BD` in UTF-8; short-circuit when absent (the common case).
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

/// Returns `true` if any quasi or interpolation in `t` may carry the lone-surrogate encoding.
///
/// Split out from [`expr_may_have_lone_surrogates`]'s `TemplateLiteral` arm so sites that hold a
/// `&TemplateLiteral` directly can reuse the check.
pub fn template_may_have_lone_surrogates<'a>(
    t: &TemplateLiteral<'a>,
    ctx: &impl GlobalContext<'a>,
) -> bool {
    t.quasis.iter().any(|q| q.lone_surrogates)
        || t.expressions.iter().any(|e| expr_may_have_lone_surrogates(e, ctx))
}

/// Returns `true` if any element in `arr` may carry the lone-surrogate encoding.
///
/// `as_expression` skips `SpreadElement` and `Elision`. Sound for the array-join / array-split
/// folds that call this: `ArrayJoin::array_join` bails on any element whose `to_js_string` fails,
/// and `SpreadElement::to_js_string` always fails — so an array with a spread never produces a
/// `ConstantValue::String` for these folds. Elisions stringify to `""`.
///
/// Split out from [`expr_may_have_lone_surrogates`]'s `ArrayExpression` arm so sites that hold an
/// `&ArrayExpression` directly can reuse the check.
pub fn array_may_have_lone_surrogates<'a>(
    arr: &ArrayExpression<'a>,
    ctx: &impl GlobalContext<'a>,
) -> bool {
    arr.elements
        .iter()
        .any(|el| el.as_expression().is_some_and(|e| expr_may_have_lone_surrogates(e, ctx)))
}

/// Returns `true` if the expression, when stringified, may carry the lone-surrogate encoding.
///
/// Fold sites call this before consuming an operand's string value; when it returns `true`, the
/// caller must skip the fold or the result would be a new string literal with `lone_surrogates:
/// false`, silently corrupting the value. Conservatively over-approximates — false positives only
/// cost a missed fold.
///
/// Identifiers are resolved through `ctx.get_constant_value_for_reference_id` and the resulting
/// `ConstantValue::String` bytes are byte-scanned. That loses the AST flag but is sound for
/// bail-out: a lone-surrogate literal's bytes always contain the encoding, and a byte-identical
/// non-surrogate string only causes a missed fold. One source of such byte-identical constants
/// is a prior concat like `'�' + 'dc00'` — neither operand is flagged, the concat folds
/// legitimately, and the resulting `ConstantValue::String` bytes match the encoding pattern.
pub fn expr_may_have_lone_surrogates<'a>(
    expr: &Expression<'a>,
    ctx: &impl GlobalContext<'a>,
) -> bool {
    match expr {
        Expression::StringLiteral(s) => s.lone_surrogates,
        Expression::TemplateLiteral(t) => template_may_have_lone_surrogates(t, ctx),
        Expression::BinaryExpression(e) if e.operator == BinaryOperator::Addition => {
            expr_may_have_lone_surrogates(&e.left, ctx)
                || expr_may_have_lone_surrogates(&e.right, ctx)
        }
        Expression::ArrayExpression(arr) => array_may_have_lone_surrogates(arr, ctx),
        Expression::Identifier(ident) => ident
            .reference_id
            .get()
            .and_then(|rid| ctx.get_constant_value_for_reference_id(rid))
            .is_some_and(|cv| match cv {
                ConstantValue::String(s) => str_has_lone_surrogate_encoding(&s),
                _ => false,
            }),
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
        // The catch-all rests on two separate arguments, one per group of remaining kinds:
        //
        // (1) `UnaryExpression` (`typeof`/`void`/`!` etc.) yields fixed ASCII strings,
        //     `MemberExpression` / `ChainExpression` only fold `.length` → number, and
        //     `AssignmentExpression` / `UpdateExpression` are rejected upstream by
        //     `may_have_side_effects`. None of these can surface a lone-surrogate string value
        //     for a parent fold to consume.
        //
        // (2) `CallExpression` / `NewExpression` / `TaggedTemplateExpression` *can* produce a
        //     string (via `CallExpression::evaluate_value_to` → `try_fold_known_global_methods`,
        //     plus the `.concat` path in `replace_known_methods.rs`). We do not analyse them
        //     here; instead, each string-producing fold carries its own lone-surrogate bailout
        //     at the point it would emit a literal. Because children fold before parents, by
        //     the time a parent checks this function on a call, either the call has already
        //     been rewritten to a `StringLiteral` (caught by the first arm of this match) or
        //     it didn't fold. Returning `false` here is therefore safe *as long as* every
        //     string-producing fold preserves that invariant.
        //
        // When adding a new string-producing fold, either add its corresponding `Expression`
        // kind as a dedicated arm above, or add an explicit `expr_may_have_lone_surrogates`
        // check at the fold site before emitting a literal.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::str_has_lone_surrogate_encoding;

    #[test]
    fn empty_and_short_inputs() {
        assert!(!str_has_lone_surrogate_encoding(""));
        assert!(!str_has_lone_surrogate_encoding("abc"));
        // 6 bytes: one short of the 7-byte window.
        assert!(!str_has_lone_surrogate_encoding("\u{FFFD}dc0"));
    }

    #[test]
    fn no_u_fffd_short_circuits() {
        assert!(!str_has_lone_surrogate_encoding("plain ascii text"));
        assert!(!str_has_lone_surrogate_encoding(&"a".repeat(1024)));
    }

    #[test]
    fn surrogate_range_boundaries() {
        // Low and high surrogate boundaries match.
        assert!(str_has_lone_surrogate_encoding("\u{FFFD}d800"));
        assert!(str_has_lone_surrogate_encoding("\u{FFFD}dbff"));
        assert!(str_has_lone_surrogate_encoding("\u{FFFD}dc00"));
        assert!(str_has_lone_surrogate_encoding("\u{FFFD}dfff"));
        // Just outside the surrogate range.
        assert!(!str_has_lone_surrogate_encoding("\u{FFFD}d7ff"));
        assert!(!str_has_lone_surrogate_encoding("\u{FFFD}e000"));
    }

    #[test]
    fn self_escape_for_literal_u_fffd() {
        assert!(str_has_lone_surrogate_encoding("\u{FFFD}fffd"));
    }

    #[test]
    fn uppercase_hex_is_not_the_encoding() {
        // The encoding uses lowercase hex; `�D800` is real U+FFFD
        // followed by the ASCII text "D800".
        assert!(!str_has_lone_surrogate_encoding("\u{FFFD}D800"));
        assert!(!str_has_lone_surrogate_encoding("\u{FFFD}FFFD"));
    }

    #[test]
    fn non_hex_suffix_is_rejected() {
        // `�dz00` — "dz00" isn't a valid hex run, and isn't
        // "fffd", so no match.
        assert!(!str_has_lone_surrogate_encoding("\u{FFFD}dz00"));
        // `�d80g` — trailing non-hex.
        assert!(!str_has_lone_surrogate_encoding("\u{FFFD}d80g"));
    }

    #[test]
    fn matches_anywhere_in_string() {
        assert!(str_has_lone_surrogate_encoding("prefix\u{FFFD}d800suffix"));
        assert!(str_has_lone_surrogate_encoding("a\u{FFFD}dc00b\u{FFFD}dfffc"));
    }

    #[test]
    fn lone_u_fffd_alone_is_not_the_encoding() {
        assert!(!str_has_lone_surrogate_encoding("\u{FFFD}"));
        assert!(!str_has_lone_surrogate_encoding("hello \u{FFFD} world"));
    }
}
