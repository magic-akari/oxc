use oxc_allocator::Box;
use oxc_ast::ast::*;
use oxc_span::Span;

use super::FunctionKind;
use crate::{
    Context, ParserImpl, StatementContext, diagnostics,
    lexer::Kind,
    modifiers::{ModifierFlags, ModifierKind, Modifiers},
};

impl FunctionKind {
    pub(crate) fn is_id_required(self) -> bool {
        matches!(self, Self::Declaration)
    }

    pub(crate) fn is_expression(self) -> bool {
        self == Self::Expression
    }
}

impl<'a> ParserImpl<'a> {
    pub(crate) fn at_function_with_async(&mut self) -> bool {
        self.at(Kind::Function)
            || self.at(Kind::Async)
                && self.lookahead(|p| {
                    p.bump_any();
                    p.at(Kind::Function) && !p.token.is_on_new_line()
                })
    }

    pub(crate) fn parse_function_body(&mut self) -> Box<'a, FunctionBody<'a>> {
        let span = self.start_span();
        self.expect(Kind::LCurly);

        let (directives, statements) = self.context(Context::Return, Context::empty(), |p| {
            p.parse_directives_and_statements(/* is_top_level */ false)
        });

        self.expect(Kind::RCurly);
        self.ast.alloc_function_body(self.end_span(span), directives, statements)
    }

    pub(crate) fn parse_formal_parameters(
        &mut self,
        params_kind: FormalParameterKind,
    ) -> (Option<TSThisParameter<'a>>, Box<'a, FormalParameters<'a>>) {
        let span = self.start_span();
        self.expect(Kind::LParen);
        let this_param = if self.is_ts && self.at(Kind::This) {
            let param = self.parse_ts_this_parameter();
            if !self.at(Kind::RParen) {
                self.expect(Kind::Comma);
            }
            Some(param)
        } else {
            None
        };
        let (list, rest) = self.parse_delimited_list_with_rest(
            Kind::RParen,
            Self::parse_formal_parameter,
            diagnostics::rest_parameter_last,
        );
        self.expect(Kind::RParen);
        let formal_parameters =
            self.ast.alloc_formal_parameters(self.end_span(span), params_kind, list, rest);
        (this_param, formal_parameters)
    }

    fn parse_parameter_modifiers(&mut self) -> Modifiers<'a> {
        let modifiers = self.parse_class_element_modifiers(true);
        self.verify_modifiers(
            &modifiers,
            ModifierFlags::ACCESSIBILITY
                .union(ModifierFlags::READONLY)
                .union(ModifierFlags::OVERRIDE),
            diagnostics::cannot_appear_on_a_parameter,
        );
        modifiers
    }

    fn parse_formal_parameter(&mut self) -> FormalParameter<'a> {
        let span = self.start_span();
        if self.at(Kind::At) {
            self.eat_decorators();
        }
        let decorators = self.consume_decorators();
        let modifiers = self.parse_parameter_modifiers();
        let pattern = self.parse_binding_pattern_with_initializer();
        self.ast.formal_parameter(
            self.end_span(span),
            decorators,
            pattern,
            modifiers.accessibility(),
            modifiers.contains_readonly(),
            modifiers.contains_override(),
        )
    }

    pub(crate) fn parse_function(
        &mut self,
        span: u32,
        id: Option<BindingIdentifier<'a>>,
        r#async: bool,
        generator: bool,
        func_kind: FunctionKind,
        param_kind: FormalParameterKind,
        modifiers: &Modifiers<'a>,
    ) -> Box<'a, Function<'a>> {
        let ctx = self.ctx;
        self.ctx = self.ctx.and_in(true).and_await(r#async).and_yield(generator);

        let type_parameters = self.parse_ts_type_parameters();

        let (this_param, params) = self.parse_formal_parameters(param_kind);

        let return_type =
            self.parse_ts_return_type_annotation(Kind::Colon, /* is_type */ true);

        let body = if self.at(Kind::LCurly) { Some(self.parse_function_body()) } else { None };

        self.ctx =
            self.ctx.and_in(ctx.has_in()).and_await(ctx.has_await()).and_yield(ctx.has_yield());

        if !self.is_ts && body.is_none() {
            return self.unexpected();
        }

        let function_type = match func_kind {
            FunctionKind::Declaration | FunctionKind::DefaultExport => {
                if body.is_none() {
                    FunctionType::TSDeclareFunction
                } else {
                    FunctionType::FunctionDeclaration
                }
            }
            FunctionKind::Expression => {
                if body.is_none() {
                    FunctionType::TSEmptyBodyFunctionExpression
                } else {
                    FunctionType::FunctionExpression
                }
            }
            FunctionKind::TSDeclaration => FunctionType::TSDeclareFunction,
        };

        if FunctionType::TSDeclareFunction == function_type
            || FunctionType::TSEmptyBodyFunctionExpression == function_type
        {
            self.asi();
        }

        self.verify_modifiers(
            modifiers,
            ModifierFlags::DECLARE | ModifierFlags::ASYNC,
            diagnostics::modifier_cannot_be_used_here,
        );

        self.ast.alloc_function(
            self.end_span(span),
            function_type,
            id,
            generator,
            r#async,
            modifiers.contains_declare(),
            type_parameters,
            this_param,
            params,
            return_type,
            body,
        )
    }

    /// [Function Declaration](https://tc39.es/ecma262/#prod-FunctionDeclaration)
    pub(crate) fn parse_function_declaration(
        &mut self,
        span: u32,
        r#async: bool,
        stmt_ctx: StatementContext,
    ) -> Statement<'a> {
        let func_kind = FunctionKind::Declaration;
        let decl = self.parse_function_impl(span, r#async, func_kind);
        if stmt_ctx.is_single_statement() {
            if decl.r#async {
                self.error(diagnostics::async_function_declaration(Span::new(
                    decl.span.start,
                    decl.params.span.end,
                )));
            } else if decl.generator {
                self.error(diagnostics::generator_function_declaration(Span::new(
                    decl.span.start,
                    decl.params.span.end,
                )));
            }
        }
        Statement::FunctionDeclaration(decl)
    }

    /// Parse function implementation in Javascript, cursor
    /// at `function` or `async function`
    pub(crate) fn parse_function_impl(
        &mut self,
        span: u32,
        r#async: bool,
        func_kind: FunctionKind,
    ) -> Box<'a, Function<'a>> {
        self.expect(Kind::Function);
        let generator = self.eat(Kind::Star);
        let id = self.parse_function_id(func_kind, r#async, generator);
        self.parse_function(
            span,
            id,
            r#async,
            generator,
            func_kind,
            FormalParameterKind::FormalParameter,
            &Modifiers::empty(),
        )
    }

    /// Parse function implementation in Typescript, cursor
    /// at `function`
    pub(crate) fn parse_ts_function_impl(
        &mut self,
        start_span: u32,
        func_kind: FunctionKind,
        modifiers: &Modifiers<'a>,
    ) -> Box<'a, Function<'a>> {
        let r#async = modifiers.contains(ModifierKind::Async);
        self.expect(Kind::Function);
        let generator = self.eat(Kind::Star);
        let id = self.parse_function_id(func_kind, r#async, generator);
        self.parse_function(
            start_span,
            id,
            r#async,
            generator,
            func_kind,
            FormalParameterKind::FormalParameter,
            modifiers,
        )
    }

    /// [Function Expression](https://tc39.es/ecma262/#prod-FunctionExpression)
    pub(crate) fn parse_function_expression(&mut self, span: u32, r#async: bool) -> Expression<'a> {
        let func_kind = FunctionKind::Expression;
        self.expect(Kind::Function);

        let generator = self.eat(Kind::Star);
        let id = self.parse_function_id(func_kind, r#async, generator);
        let function = self.parse_function(
            span,
            id,
            r#async,
            generator,
            func_kind,
            FormalParameterKind::FormalParameter,
            &Modifiers::empty(),
        );
        Expression::FunctionExpression(function)
    }

    /// Section 15.4 Method Definitions
    /// `ClassElementName` ( `UniqueFormalParameters` ) { `FunctionBody` }
    /// `GeneratorMethod`
    ///   * `ClassElementName`
    /// `AsyncMethod`
    ///   async `ClassElementName`
    /// `AsyncGeneratorMethod`
    ///   async * `ClassElementName`
    pub(crate) fn parse_method(&mut self, r#async: bool, generator: bool) -> Box<'a, Function<'a>> {
        let span = self.start_span();
        self.parse_function(
            span,
            None,
            r#async,
            generator,
            FunctionKind::Expression,
            FormalParameterKind::UniqueFormalParameters,
            &Modifiers::empty(),
        )
    }

    /// Section 15.5 Yield Expression
    /// yield
    /// yield [no `LineTerminator` here] `AssignmentExpression`
    /// yield [no `LineTerminator` here] * `AssignmentExpression`
    pub(crate) fn parse_yield_expression(&mut self) -> Expression<'a> {
        let span = self.start_span();
        self.bump_any(); // advance `yield`

        let has_yield = self.ctx.has_yield();
        if !has_yield {
            self.error(diagnostics::yield_expression(Span::new(span, span + 5)));
        }

        let mut delegate = false;
        let mut argument = None;

        if !self.cur_token().is_on_new_line() {
            delegate = self.eat(Kind::Star);
            let not_assignment_expr = matches!(
                self.cur_kind(),
                Kind::Semicolon
                    | Kind::Eof
                    | Kind::RCurly
                    | Kind::RParen
                    | Kind::RBrack
                    | Kind::Colon
                    | Kind::Comma
            );
            if !not_assignment_expr || delegate {
                self.ctx = self.ctx.union_yield_if(true);
                argument = Some(self.parse_assignment_expression_or_higher());
                self.ctx = self.ctx.and_yield(has_yield);
            }
        }

        self.ast.expression_yield(self.end_span(span), delegate, argument)
    }

    // id: None - for AnonymousDefaultExportedFunctionDeclaration
    pub(crate) fn parse_function_id(
        &mut self,
        func_kind: FunctionKind,
        r#async: bool,
        generator: bool,
    ) -> Option<BindingIdentifier<'a>> {
        let kind = self.cur_kind();
        if kind.is_binding_identifier() {
            let mut ctx = self.ctx;
            if func_kind.is_expression() {
                ctx = ctx.and_await(r#async).and_yield(generator);
            }
            self.check_identifier(kind, ctx);

            let (span, name) = self.parse_identifier_kind(Kind::Ident);
            Some(self.ast.binding_identifier(span, name))
        } else {
            if func_kind.is_id_required() {
                match self.cur_kind() {
                    Kind::LParen => {
                        self.error(diagnostics::expect_function_name(self.cur_token().span()));
                    }
                    kind if kind.is_reserved_keyword() => self.expect_without_advance(Kind::Ident),
                    _ => {}
                }
            }

            None
        }
    }
}
