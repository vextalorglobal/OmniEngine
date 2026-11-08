//! A flat AST: every node is an index into one vector.
//!
//! The fused evaluator this replaces had no tree at all, so Phase 6 is free to
//! choose a shape. A pointer-per-node tree would allocate once per literal and
//! once per operator, which would break the warm-execution tests that count
//! allocation requests. A `Vec<Node>` reuses its capacity instead: building a
//! program clears the vector and pushes the same number of nodes, and after the
//! first execution of a given size the system allocator is not asked again.
//!
//! Children are indices, not pointers, so the vector may grow without
//! invalidating a node. Call arguments are a singly linked list through
//! [`Node::next`]; nothing owns a buffer of children.
//!
//! The tree records spans, not decoded values. A number is parsed and a string
//! is unescaped when its instruction executes, which is exactly when the fused
//! evaluator did it — so error order relative to arithmetic is unchanged.

use crate::engine::{ErrorKind, MAX_ARGS, MAX_CALL_DEPTH, OmniError, Pos};
use crate::lexer::{BinOp, Token, TokenKind};

/// "No node" in an index field.
pub(crate) const NONE: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeKind {
    /// A decimal literal; `start`/`len` address the source text.
    Number,
    /// A string literal; `start`/`len` address the *contents*, quotes excluded.
    Str,
    /// A bare identifier.
    Var,
    /// `base.member`, the only member form the grammar has.
    Member,
    /// `let name = value`; `start`/`len` name the binding, `lhs` is the value.
    Let,
    /// `import "path"`; `start`/`len` address the literal's raw contents.
    Import,
    /// Unary minus; `lhs` is the operand.
    UnaryNeg,
    /// One of `+ - * / %`; `lhs` and `rhs` are the operands.
    Binary(BinOp),
    /// `==`.
    Eq,
    /// `!=`.
    Ne,
    /// `name(arg, …)`; `lhs` is the first argument, `rhs` the argument count.
    Call,
    /// `base.member(arg, …)`; the name fields hold the two identifiers.
    CallMember,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Node {
    pub kind: NodeKind,
    pub pos: Pos,
    /// Primary span in the source: literal contents, identifier, or binding name.
    pub start: u32,
    pub len: u32,
    /// Second identifier, for [`NodeKind::Member`] and [`NodeKind::CallMember`].
    pub start2: u32,
    pub len2: u32,
    /// Left child, operand, or first argument.
    pub lhs: u32,
    /// Right child, or the argument count for a call.
    pub rhs: u32,
    /// Next argument in a call's sibling chain.
    pub next: u32,
}

impl Node {
    fn leaf(kind: NodeKind, pos: Pos, start: u32, len: u32) -> Node {
        Node {
            kind,
            pos,
            start,
            len,
            start2: 0,
            len2: 0,
            lhs: NONE,
            rhs: 0,
            next: NONE,
        }
    }
}

/// One parsed program. Both vectors are reused across executions.
#[derive(Debug, Default)]
pub(crate) struct Ast {
    pub nodes: Vec<Node>,
    pub statements: Vec<u32>,
}

impl Ast {
    pub(crate) fn new() -> Ast {
        Ast::default()
    }

    /// Parse into this tree, replacing whatever it held.
    ///
    /// The tree is cleared even when parsing fails, so a caller that ignored the
    /// `Result` cannot walk half a program — the same all-or-nothing contract
    /// `lex_into` gives its token buffer.
    pub(crate) fn parse_into(&mut self, source: &str, tokens: &[Token]) -> Result<(), OmniError> {
        self.nodes.clear();
        self.statements.clear();
        let mut parser = Parser {
            source,
            tokens,
            index: 0,
            call_depth: 0,
            ast: self,
        };
        parser.program()
    }

    fn push(&mut self, node: Node) -> u32 {
        let index = self.nodes.len() as u32;
        self.nodes.push(node);
        index
    }
}

struct Parser<'s, 'a> {
    source: &'s str,
    tokens: &'s [Token],
    index: usize,
    /// Syntactic call nesting, checked the way the recursive parser checked it.
    call_depth: u32,
    ast: &'a mut Ast,
}

impl Parser<'_, '_> {
    fn program(&mut self) -> Result<(), OmniError> {
        loop {
            while self.kind(self.index) == Some(TokenKind::Semi) {
                self.index += 1;
            }
            match self.kind(self.index) {
                None | Some(TokenKind::Eof) => break,
                Some(TokenKind::Let) => {
                    self.index += 1;
                    let name = self.expect_ident()?;
                    let name_pos = self.pos(name);
                    self.expect(TokenKind::Eq)?;
                    let value = self.expr()?;
                    let index = self.ast.push(Node {
                        kind: NodeKind::Let,
                        pos: name_pos,
                        start: name.start,
                        len: name.len,
                        lhs: value,
                        ..Node::leaf(NodeKind::Let, name_pos, name.start, name.len)
                    });
                    self.ast.statements.push(index);
                }
                Some(TokenKind::Import) => {
                    let keyword = self.token(self.index).expect("peeked above");
                    self.index += 1;
                    let path = match self.token(self.index) {
                        Some(token) if token.kind == TokenKind::Str => token,
                        Some(token) => {
                            return Err(OmniError::new(
                                ErrorKind::Parse,
                                self.pos(token),
                                "import needs a string literal naming a module",
                            ));
                        }
                        None => {
                            return Err(OmniError::new(
                                ErrorKind::Parse,
                                self.end_pos(),
                                "import needs a string literal naming a module",
                            ));
                        }
                    };
                    self.index += 1;
                    let index = self.ast.push(Node::leaf(
                        NodeKind::Import,
                        self.pos(keyword),
                        path.start,
                        path.len,
                    ));
                    self.ast.statements.push(index);
                }
                _ => {
                    let expression = self.expr()?;
                    self.ast.statements.push(expression);
                }
            }
        }
        Ok(())
    }

    /// `==` and `!=`, one level below `+`, exactly as the fused parser had them.
    fn expr(&mut self) -> Result<u32, OmniError> {
        let mut acc = self.additive()?;
        loop {
            let kind = match self.kind(self.index) {
                Some(TokenKind::EqEq) => NodeKind::Eq,
                Some(TokenKind::NotEq) => NodeKind::Ne,
                _ => break,
            };
            let at = self.pos_at(self.index);
            self.index += 1;
            let rhs = self.additive()?;
            acc = self.ast.push(Node {
                kind,
                pos: at,
                lhs: acc,
                rhs,
                ..Node::leaf(kind, at, 0, 0)
            });
        }
        Ok(acc)
    }

    fn additive(&mut self) -> Result<u32, OmniError> {
        let mut acc = self.term()?;
        loop {
            let op = match self.kind(self.index) {
                Some(TokenKind::BinOp(BinOp::Add)) => BinOp::Add,
                Some(TokenKind::BinOp(BinOp::Sub)) => BinOp::Sub,
                _ => break,
            };
            let at = self.pos_at(self.index);
            self.index += 1;
            let rhs = self.term()?;
            acc = self.ast.push(Node {
                kind: NodeKind::Binary(op),
                pos: at,
                lhs: acc,
                rhs,
                ..Node::leaf(NodeKind::Binary(op), at, 0, 0)
            });
        }
        Ok(acc)
    }

    fn term(&mut self) -> Result<u32, OmniError> {
        let mut acc = self.factor()?;
        loop {
            let op = match self.kind(self.index) {
                Some(TokenKind::BinOp(BinOp::Mul)) => BinOp::Mul,
                Some(TokenKind::BinOp(BinOp::Div)) => BinOp::Div,
                Some(TokenKind::BinOp(BinOp::Mod)) => BinOp::Mod,
                _ => break,
            };
            let at = self.pos_at(self.index);
            self.index += 1;
            let rhs = self.factor()?;
            acc = self.ast.push(Node {
                kind: NodeKind::Binary(op),
                pos: at,
                lhs: acc,
                rhs,
                ..Node::leaf(NodeKind::Binary(op), at, 0, 0)
            });
        }
        Ok(acc)
    }

    /// Unary signs, folded before the operand as the fused parser did. `--5` is
    /// `5` and `- 2 * 3` is `-6`; the sign is not an operator node of its own
    /// unless it survives as a single negation.
    fn factor(&mut self) -> Result<u32, OmniError> {
        let mut negate = false;
        loop {
            match self.kind(self.index) {
                Some(TokenKind::BinOp(BinOp::Sub)) => {
                    negate = !negate;
                    self.index += 1;
                }
                Some(TokenKind::BinOp(BinOp::Add)) => self.index += 1,
                _ => break,
            }
        }
        let operand_pos = self.pos_at(self.index);
        let operand = self.primary()?;
        if negate {
            return Ok(self.ast.push(Node {
                kind: NodeKind::UnaryNeg,
                pos: operand_pos,
                lhs: operand,
                ..Node::leaf(NodeKind::UnaryNeg, operand_pos, 0, 0)
            }));
        }
        Ok(operand)
    }

    fn primary(&mut self) -> Result<u32, OmniError> {
        let token = match self.token(self.index) {
            Some(token) => token,
            None => {
                return Err(OmniError::new(
                    ErrorKind::Parse,
                    self.end_pos(),
                    "unexpected end of input",
                ));
            }
        };
        match token.kind {
            TokenKind::Number => {
                self.index += 1;
                Ok(self.ast.push(Node::leaf(
                    NodeKind::Number,
                    self.pos(token),
                    token.start,
                    token.len,
                )))
            }
            TokenKind::Str => {
                self.index += 1;
                Ok(self.ast.push(Node::leaf(
                    NodeKind::Str,
                    self.pos(token),
                    token.start,
                    token.len,
                )))
            }
            TokenKind::Ident => {
                self.index += 1;
                match self.kind(self.index) {
                    Some(TokenKind::LParen) => self.call(token, None),
                    Some(TokenKind::Dot) => {
                        self.index += 1;
                        let member = self.expect_ident()?;
                        if self.kind(self.index) == Some(TokenKind::LParen) {
                            return self.call(token, Some(member));
                        }
                        let at = self.pos(member);
                        Ok(self.ast.push(Node {
                            kind: NodeKind::Member,
                            pos: at,
                            start: token.start,
                            len: token.len,
                            start2: member.start,
                            len2: member.len,
                            ..Node::leaf(NodeKind::Member, at, token.start, token.len)
                        }))
                    }
                    _ => Ok(self.ast.push(Node::leaf(
                        NodeKind::Var,
                        self.pos(token),
                        token.start,
                        token.len,
                    ))),
                }
            }
            TokenKind::LParen => {
                self.index += 1;
                let inner = self.expr()?;
                self.expect(TokenKind::RParen)?;
                Ok(inner)
            }
            TokenKind::Eof => Err(OmniError::new(
                ErrorKind::Parse,
                self.pos(token),
                "unexpected end of input",
            )),
            other => Err(self.unexpected(other, "expected a value")),
        }
    }

    /// `name(args)` or `base.member(args)` with the opening paren at the cursor.
    ///
    /// The callee is *not* resolved here. Existence is checked by the lowering
    /// pass before it walks the arguments, which is the same order the fused
    /// parser used, but it keeps the parser free of engine state and lets a
    /// chunk be inspected without a host registry.
    fn call(&mut self, base: Token, member: Option<Token>) -> Result<u32, OmniError> {
        let at = self.pos(base);
        if self.call_depth >= MAX_CALL_DEPTH {
            return Err(OmniError::new(
                ErrorKind::Runtime,
                at,
                format!(
                    "calls are nested {MAX_CALL_DEPTH} deep, the limit; a host function \
                     cannot call itself"
                ),
            ));
        }
        self.index += 1; // '('
        self.call_depth += 1;
        let result = self.read_args(base, member);
        self.call_depth -= 1;
        let (first, count) = result?;
        let (kind, start, len, start2, len2) = match member {
            None => (NodeKind::Call, base.start, base.len, 0, 0),
            Some(member) => (
                NodeKind::CallMember,
                base.start,
                base.len,
                member.start,
                member.len,
            ),
        };
        Ok(self.ast.push(Node {
            kind,
            pos: at,
            start,
            len,
            start2,
            len2,
            lhs: first,
            rhs: count,
            next: NONE,
        }))
    }

    /// Read `arg, arg, …` through the closing paren.
    ///
    /// The argument list is capped by [`MAX_ARGS`] at the argument that breaks
    /// it, so the message lands on the seventeenth value rather than on the
    /// call's name. Each argument's `next` links to the one after it.
    fn read_args(&mut self, base: Token, member: Option<Token>) -> Result<(u32, u32), OmniError> {
        let mut first = NONE;
        let mut previous = NONE;
        let mut count = 0u32;
        if self.kind(self.index) != Some(TokenKind::RParen) {
            loop {
                let arg_pos = self.pos_at(self.index);
                let argument = self.expr()?;
                if previous == NONE {
                    first = argument;
                } else {
                    self.ast.nodes[previous as usize].next = argument;
                }
                previous = argument;
                count += 1;
                if count as usize > MAX_ARGS {
                    return Err(OmniError::new(
                        ErrorKind::Parse,
                        arg_pos,
                        format!("a call takes at most {MAX_ARGS} arguments"),
                    ));
                }
                match self.kind(self.index) {
                    Some(TokenKind::Comma) => self.index += 1,
                    Some(TokenKind::RParen) | None => break,
                    Some(kind) => {
                        // Only this branch formats the name, so a successful
                        // parse of `f(1)` allocates nothing for the message.
                        let shown = match member {
                            None => self.text(base).to_string(),
                            Some(member) => {
                                format!("{}.{}", self.text(base), self.text(member))
                            }
                        };
                        return Err(OmniError::new(
                            ErrorKind::Parse,
                            self.pos_at(self.index),
                            format!(
                                "expected ',' or ')' in the call to '{shown}' (found '{}')",
                                display_kind(kind)
                            ),
                        ));
                    }
                }
            }
        }
        self.expect(TokenKind::RParen)?;
        Ok((first, count))
    }

    fn expect(&mut self, want: TokenKind) -> Result<(), OmniError> {
        match self.token(self.index) {
            Some(token) if token.kind == want => {
                self.index += 1;
                Ok(())
            }
            Some(token) => Err(OmniError::new(
                ErrorKind::Parse,
                self.pos(token),
                format!("unexpected token (found '{}')", display_kind(token.kind)),
            )),
            None => Err(OmniError::new(
                ErrorKind::Parse,
                self.end_pos(),
                "unexpected end of input",
            )),
        }
    }

    fn expect_ident(&mut self) -> Result<Token, OmniError> {
        match self.token(self.index) {
            Some(token) if token.kind == TokenKind::Ident => {
                self.index += 1;
                Ok(token)
            }
            Some(token) => Err(OmniError::new(
                ErrorKind::Parse,
                self.pos(token),
                format!("expected a name, found '{}'", display_kind(token.kind)),
            )),
            None => Err(OmniError::new(
                ErrorKind::Parse,
                self.end_pos(),
                "expected a name",
            )),
        }
    }

    fn unexpected(&self, kind: TokenKind, context: &str) -> OmniError {
        OmniError::new(
            ErrorKind::Parse,
            self.pos_at(self.index),
            format!("{context} (found '{}')", display_kind(kind)),
        )
    }

    fn token(&self, index: usize) -> Option<Token> {
        self.tokens.get(index).copied()
    }

    fn kind(&self, index: usize) -> Option<TokenKind> {
        self.tokens.get(index).map(|token| token.kind)
    }

    fn text(&self, token: Token) -> &str {
        token.text(self.source)
    }

    fn pos(&self, token: Token) -> Pos {
        Pos::new(token.line, column_at(self.source, token.start as usize))
    }

    fn pos_at(&self, index: usize) -> Pos {
        match self.tokens.get(index) {
            Some(token) => self.pos(*token),
            None => self.end_pos(),
        }
    }

    fn end_pos(&self) -> Pos {
        let line = self.tokens.last().map_or(1, |token| token.line);
        let column = self
            .source
            .rsplit('\n')
            .next()
            .map_or(1, |tail| tail.len() as u32 + 1);
        Pos::new(line, column)
    }
}

/// One-based byte column of `offset` in `source`.
fn column_at(source: &str, offset: usize) -> u32 {
    let Some(before) = source.get(..offset) else {
        return 1;
    };
    let line_start = before.rfind('\n').map_or(0, |newline| newline + 1);
    (offset - line_start + 1) as u32
}

pub(crate) fn display_kind(kind: TokenKind) -> &'static str {
    match kind {
        TokenKind::Let => "'let'",
        TokenKind::Import => "'import'",
        TokenKind::Ident => "a name",
        TokenKind::Number => "a number",
        TokenKind::Str => "a string",
        TokenKind::Eq => "'='",
        TokenKind::EqEq => "'=='",
        TokenKind::NotEq => "'!='",
        TokenKind::LParen => "'('",
        TokenKind::RParen => "')'",
        TokenKind::Dot => "'.'",
        TokenKind::Comma => "','",
        TokenKind::Semi => "';'",
        TokenKind::Eof => "end of input",
        TokenKind::BinOp(op) => op.symbol(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    fn parse(source: &str) -> Result<Ast, OmniError> {
        let tokens = lex(source).expect("lexed");
        let mut ast = Ast::new();
        ast.parse_into(source, &tokens)?;
        Ok(ast)
    }

    fn tree(source: &str) -> Ast {
        parse(source).unwrap_or_else(|e| panic!("{source} failed: {e}"))
    }

    #[test]
    fn precedence_is_a_tree_shape_not_an_evaluation_order() {
        let ast = tree("1 + 2 * 3");
        assert_eq!(ast.statements.len(), 1);
        let root = &ast.nodes[ast.statements[0] as usize];
        assert_eq!(root.kind, NodeKind::Binary(BinOp::Add));
        let left = &ast.nodes[root.lhs as usize];
        let right = &ast.nodes[root.rhs as usize];
        assert_eq!(left.kind, NodeKind::Number);
        assert_eq!(right.kind, NodeKind::Binary(BinOp::Mul));
        assert_eq!(right.pos, Pos::new(1, 7), "the '*' carries the operator");
    }

    #[test]
    fn let_is_a_statement_whose_value_is_the_binding() {
        let ast = tree("let answer = 40 + 2");
        let root = &ast.nodes[ast.statements[0] as usize];
        assert_eq!(root.kind, NodeKind::Let);
        assert_eq!(
            &"let answer = 40 + 2"[root.start as usize..][..root.len as usize],
            "answer"
        );
        assert_eq!(
            ast.nodes[root.lhs as usize].kind,
            NodeKind::Binary(BinOp::Add)
        );
    }

    #[test]
    fn call_arguments_are_a_sibling_chain_with_a_count() {
        let ast = tree("clamp(12, 0, 10)");
        let root = &ast.nodes[ast.statements[0] as usize];
        assert_eq!(root.kind, NodeKind::Call);
        assert_eq!(root.rhs, 3);
        let first = &ast.nodes[root.lhs as usize];
        let second = &ast.nodes[first.next as usize];
        let third = &ast.nodes[second.next as usize];
        assert_eq!(third.next, NONE);
        assert_eq!(
            &"clamp(12, 0, 10)"[second.start as usize..][..second.len as usize],
            "0"
        );
    }

    #[test]
    fn a_qualified_name_is_a_member_node() {
        let ast = tree("math.answer");
        let root = &ast.nodes[ast.statements[0] as usize];
        assert_eq!(root.kind, NodeKind::Member);
        assert_eq!(
            &"math.answer"[root.start as usize..][..root.len as usize],
            "math"
        );
        assert_eq!(
            &"math.answer"[root.start2 as usize..][..root.len2 as usize],
            "answer"
        );
    }

    #[test]
    fn a_qualified_call_keeps_both_identifiers() {
        let ast = tree("fs.read_text(\"x\")");
        let root = &ast.nodes[ast.statements[0] as usize];
        assert_eq!(root.kind, NodeKind::CallMember);
        assert_eq!(root.rhs, 1);
        assert_eq!(
            &"fs.read_text(\"x\")"[root.start as usize..][..root.len as usize],
            "fs"
        );
        assert_eq!(
            &"fs.read_text(\"x\")"[root.start2 as usize..][..root.len2 as usize],
            "read_text"
        );
    }

    #[test]
    fn import_is_its_own_statement() {
        let ast = tree("import \"lib/math\"\nmath.answer");
        assert_eq!(ast.statements.len(), 2);
        let import = &ast.nodes[ast.statements[0] as usize];
        assert_eq!(import.kind, NodeKind::Import);
        assert_eq!(
            &"import \"lib/math\"\nmath.answer"[import.start as usize..][..import.len as usize],
            "lib/math"
        );
    }

    #[test]
    fn unary_signs_fold_before_the_operand() {
        let ast = tree("--5");
        let root = &ast.nodes[ast.statements[0] as usize];
        assert_eq!(root.kind, NodeKind::Number, "two minus signs cancel");

        let ast = tree("- 2 * 3");
        let root = &ast.nodes[ast.statements[0] as usize];
        assert_eq!(root.kind, NodeKind::Binary(BinOp::Mul));
        assert_eq!(ast.nodes[root.lhs as usize].kind, NodeKind::UnaryNeg);
        assert_eq!(
            ast.nodes[root.lhs as usize].pos,
            Pos::new(1, 3),
            "the operand's own position, as the old error pointed"
        );
    }

    #[test]
    fn empty_programs_parse_to_no_statements() {
        assert!(tree("").statements.is_empty());
        assert!(tree(";;").statements.is_empty());
    }

    #[test]
    fn an_import_needs_a_string_literal() {
        let e = parse("import math").unwrap_err();
        assert_eq!(e.kind, ErrorKind::Parse);
        assert_eq!(e.message, "import needs a string literal naming a module");
    }

    #[test]
    fn parse_errors_keep_their_wording() {
        let e = parse("1 +").unwrap_err();
        assert_eq!(e.kind, ErrorKind::Parse);
        assert_eq!(e.message, "unexpected end of input");
        let e = parse("let = 3").unwrap_err();
        // The doubled quotes around a symbol are the documented cosmetic defect
        // (`display_kind` quotes, the template quotes again); the wording and
        // position are what this test pins.
        assert!(e.message.starts_with("expected a name, found '"), "{e}");
        let e = parse("(1 + 2").unwrap_err();
        assert_eq!(e.message, "unexpected token (found 'end of input')");
        let e = parse("1 + )").unwrap_err();
        assert!(e.message.starts_with("expected a value (found '"), "{e}");
    }

    #[test]
    fn a_missing_comma_is_reported_at_the_comma() {
        let e = parse("square(1 2)").unwrap_err();
        assert_eq!(
            e.message,
            "expected ',' or ')' in the call to 'square' (found 'a number')"
        );
    }

    #[test]
    fn more_than_sixteen_arguments_is_a_parse_error() {
        let source = format!("square({})", vec!["1"; MAX_ARGS + 1].join(","));
        let e = parse(&source).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Parse);
        assert_eq!(
            e.message,
            format!("a call takes at most {MAX_ARGS} arguments")
        );
    }

    #[test]
    fn syntactic_call_nesting_stops_at_the_limit() {
        let source = format!(
            "{}1{}",
            "id(".repeat(MAX_CALL_DEPTH as usize + 1),
            ")".repeat(MAX_CALL_DEPTH as usize + 1)
        );
        let e = parse(&source).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Runtime);
        assert!(
            e.message.contains(&format!("nested {MAX_CALL_DEPTH} deep")),
            "{e}"
        );
        let ok = format!(
            "{}1{}",
            "id(".repeat(MAX_CALL_DEPTH as usize),
            ")".repeat(MAX_CALL_DEPTH as usize)
        );
        assert!(parse(&ok).is_ok(), "at the limit still parses");
    }
}
