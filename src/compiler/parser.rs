#![allow(dead_code)]
#![allow(clippy::ptr_arg)]

use super::{reader::RantTokenReader, lexer::RantToken, message::*, Problem, Reporter};
use crate::{InternalString, RantProgramInfo, lang::*};
use fnv::FnvBuildHasher;
use line_col::LineColLookup;
use quickscope::ScopeMap;
use std::{collections::{HashMap, HashSet}, ops::Range, rc::Rc};
use RantToken::*;

type ParseResult<T> = Result<T, ()>;

const MAIN_PROGRAM_SCOPE_NAME: &str = "main scope";

// Keyword names
const KW_RETURN: &str = "return";
const KW_BREAK: &str = "break";
const KW_CONTINUE: &str = "continue";
const KW_WEIGHT: &str = "weight";
const KW_TRUE: &str = "true";
const KW_FALSE: &str = "false";
const KW_ADD: &str = "add";
const KW_SUB: &str = "sub";
const KW_MUL: &str = "mul";
const KW_DIV: &str = "div";
const KW_MOD: &str = "mod";
const KW_AND: &str = "and";
const KW_OR: &str = "or";
const KW_NOT: &str = "not";
const KW_XOR: &str = "xor";
const KW_MANY: &str = "many";
const KW_EQUAL: &str = "eq";
const KW_NOT_EQUAL: &str = "neq";
const KW_GREATER: &str = "gt";
const KW_GREATER_OR_EQUAL: &str = "ge";
const KW_LESS: &str = "lt";
const KW_LESS_OR_EQUAL: &str = "le";

/// Provides context to the sequence parser; determines valid terminating tokens among other context-sensitive features.
#[derive(Copy, Clone, PartialEq)]
enum SequenceParseMode {
  /// Parse a sequence like a top-level program.
  ///
  /// Breaks on EOF.
  TopLevel,
  /// Parse a sequence like a block element.
  ///
  /// Breaks on `Pipe`, `Colon`, and `RightBrace`.
  BlockElement,
  /// Parse a sequence like a function argument.
  ///
  /// Breaks on `Semi`, `PipeOp`, and `RightBracket`.
  FunctionArg,
  /// Parse a sequence like a function body.
  ///
  /// Breaks on `RightBrace`.
  FunctionBodyBlock,
  /// Parse a sequence like a dynamic key expression.
  ///
  /// Breaks on `RightBrace`.
  DynamicKey,
  /// Parse a sequence like an anonymous function expression.
  ///
  /// Breaks on `Colon` and `RightBracket`.
  AnonFunctionExpr,
  /// Parse a sequence like a variable assignment value.
  ///
  /// Breaks on `RightAngle` and `Semi`.
  VariableAssignment,
  /// Parse a sequence like an accessor fallback value.
  ///
  /// Breaks on `RightAngle` and `Semi`.
  AccessorFallbackValue,
  /// Parses a sequence like a parameter default value.
  ///
  /// Breaks on `RightBracket` and `Semi`.
  ParamDefaultValue,
  /// Parse a sequence like a collection initializer element.
  ///
  /// Breaks on `Semi` and `RightParen`.
  CollectionInit,
  /// Parses a single item only.
  ///
  /// Breaks automatically or on EOF.
  SingleItem,
}

/// What type of collection initializer to parse?
enum CollectionInitKind {
  /// Parse a list
  List,
  /// Parse a map
  Map
}

/// Indicates what kind of token terminated a sequence read.
enum SequenceEndType {
  /// Top-level program sequence was terminated by end-of-file.
  ProgramEnd,
  /// Block element sequence is key and was terminated by `Colon`.
  BlockAssocDelim,
  /// Block element sequence was terminated by `Pipe`.
  BlockDelim,
  /// Block element sequence was terminated by `RightBrace`.
  BlockEnd,
  /// Function argument sequence was terminated by `Semi`.
  FunctionArgEndNext,
  /// Function argument sequence was terminated by `RightBracket`.
  FunctionArgEndBreak,
  /// Function argument sequence was terminated by `PipeOp`.
  FunctionArgEndToPipe,
  /// Function body sequence was terminated by `RightBrace`.
  FunctionBodyEnd,
  /// Dynamic key sequencce was terminated by `RightBrace`.
  DynamicKeyEnd,
  /// Anonymous function expression was terminated by `Colon`.
  AnonFunctionExprToArgs,
  /// Anonymous function expression was terminated by `RightBracket` and does not expect arguments.
  AnonFunctionExprNoArgs,
  /// Anonymous function expression was terminated by `PipeOp`.
  AnonFunctionExprToPipe,
  /// Variable accessor was terminated by `RightAngle`.
  VariableAccessEnd,
  /// Variable assignment expression was terminated by `Semi`. 
  VariableAssignDelim,
  /// Accessor fallback value was termianted by `RightAngle`.
  AccessorFallbackValueToEnd,
  /// Accessor fallback value was terminated by `Semi`.
  AccessorFallbackValueToDelim,
  /// Collection initializer was terminated by `RightParen`.
  CollectionInitEnd,
  /// Collection initializer was termianted by `Semi`.
  CollectionInitDelim,
  /// A single item was parsed using `SingleItem` mode.
  SingleItemEnd,
  /// Parameter default value was terminated by `Semi`, indicating another parameter follows.
  ParamDefaultValueSeparator,
  /// Parameter default value was terminated by `RightBracket`, indicating the end of the signature was reached..
  ParamDefaultValueSignatureEnd,
}

/// Used to track variable usages during compilation.
struct VarStats {
  def_span: Range<usize>,
  writes: usize,
  reads: usize,
  /// Indicates whether the reads from this variable are fallible (meaning the variable isn't guaranteed to be defined).
  ///
  /// For optional parameters without a fallback this is `true`.
  has_fallible_read: bool,
  is_const: bool,
  role: VarRole,
}

impl VarStats {
  #[inline]
  fn add_write(&mut self) {
    self.writes += 1;
  }

  #[inline]
  fn add_read(&mut self, is_fallible_read: bool) {
    if matches!(self.role, VarRole::FallibleOptionalArgument) && is_fallible_read {
      self.has_fallible_read = true;
    }
    self.reads += 1;
  }
}

#[derive(Copy, Clone, PartialEq)]
enum VarRole {
  Normal,
  Function,
  Argument,
  FallibleOptionalArgument,
  PipeValue,
}

/// Returns a range that encompasses both input ranges.
#[inline]
fn super_range(a: &Range<usize>, b: &Range<usize>) -> Range<usize> {
  a.start.min(b.start)..a.end.max(b.end)
}

#[derive(Debug)]
enum ParsedSequenceExtras {
  WeightedBlockElement {
    weight_expr: Rc<Sequence>
  }
}

/// Contains information about a successfully parsed sequence and its context.
struct ParsedSequence {
  sequence: Sequence,
  end_type: SequenceEndType,
  is_text: bool,
  extras: Option<ParsedSequenceExtras>,
}

/// A parser that turns Rant code into an RST (Rant Syntax Tree).
pub struct RantParser<'source, 'report, R: Reporter> {
  /// A string slice containing the source code being parsed.
  source: &'source str,
  /// Flag set if there are compiler errors.
  has_errors: bool,
  /// The token stream used by the parser.
  reader: RantTokenReader<'source>,
  /// The line/col lookup for error reporting.
  lookup: LineColLookup<'source>,
  /// The error reporter.
  reporter: &'report mut R,
  /// Enables additional debug information.
  debug_enabled: bool,
  /// A string describing the origin (containing program) of a program element.
  info: Rc<RantProgramInfo>,
  /// Keeps track of active variables in each scope while parsing.
  var_stack: ScopeMap<Identifier, VarStats>,
  /// Keeps track of active variable capture frames.
  capture_stack: Vec<(usize, HashSet<Identifier, FnvBuildHasher>)>,
}

impl<'source, 'report, R: Reporter> RantParser<'source, 'report, R> {
  pub fn new(source: &'source str, reporter: &'report mut R, debug_enabled: bool, info: &Rc<RantProgramInfo>) -> Self {
    Self {
      source,
      has_errors: false,
      reader: RantTokenReader::new(source),
      lookup: LineColLookup::new(source),
      reporter,
      debug_enabled,
      info: Rc::clone(info),
      var_stack: Default::default(),
      capture_stack: Default::default(),
    }
  }
}

impl<'source, 'report, R: Reporter> RantParser<'source, 'report, R> {
  /// Top-level parsing function invoked by the compiler.
  pub fn parse(&mut self) -> Result<Rc<Sequence>, ()> {
    let result = self.parse_sequence(SequenceParseMode::TopLevel);
    match result {
      // Err if parsing "succeeded" but there are soft syntax errors
      Ok(..) if self.has_errors => Err(()),
      // Ok if parsing succeeded and there are no syntax errors
      Ok(ParsedSequence { sequence, .. }) => Ok(Rc::new(sequence)),
      // Err on hard syntax error
      Err(()) => Err(())
    }
  }
  
  /// Reports a syntax error, allowing parsing to continue but causing the final compilation to fail. 
  fn report_error(&mut self, problem: Problem, span: &Range<usize>) {
    let (line, col) = self.lookup.get(span.start);
    self.has_errors = true;
    self.reporter.report(CompilerMessage::new(problem, Severity::Error, Some(Position::new(line, col, span.clone()))));
  }

  /// Reports a warning, but allows compiling to succeed.
  fn report_warning(&mut self, problem: Problem, span: &Range<usize>) {
    let (line, col) = self.lookup.get(span.start);
    self.reporter.report(CompilerMessage::new(problem, Severity::Warning, Some(Position::new(line, col, span.clone()))));
  }
  
  /// Emits an "unexpected token" error for the most recently read token.
  #[inline]
  fn unexpected_last_token_error(&mut self) {
    self.report_error(Problem::UnexpectedToken(self.reader.last_token_string().to_string()), &self.reader.last_token_span())
  }

  /// Parses a sequence of items. Items are individual elements of a Rant program (fragments, blocks, function calls, etc.)
  #[inline]
  fn parse_sequence(&mut self, mode: SequenceParseMode) -> ParseResult<ParsedSequence> {
    self.var_stack.push_layer();
    let parse_result = self.parse_sequence_inner(mode);
    self.analyze_top_vars();
    self.var_stack.pop_layer();
    parse_result
  }
  
  /// Inner logic of `parse_sequence()`. Intended to be wrapped in other specialized sequence-parsing functions.
  #[inline(always)]
  fn parse_sequence_inner(&mut self, mode: SequenceParseMode) -> ParseResult<ParsedSequence> {    
    let mut sequence = Sequence::empty(&self.info);
    let mut next_print_flag = PrintFlag::None;
    let mut last_print_flag_span: Option<Range<usize>> = None;
    let mut is_seq_printing = false;
    let mut pending_whitespace = None;
    let debug = self.debug_enabled;

    macro_rules! check_dangling_printflags {
      () => {
        // Make sure there are no dangling printflags
        match next_print_flag {
          PrintFlag::None => {},
          PrintFlag::Hint => {
            if let Some(flag_span) = last_print_flag_span.take() {
              self.report_error(Problem::InvalidHint, &flag_span);
            }
          },
          PrintFlag::Sink => {
            if let Some(flag_span) = last_print_flag_span.take() {
              self.report_error(Problem::InvalidSink, &flag_span);
            }
          }
        }
      }
    }
    
    while let Some((token, span)) = self.reader.next() {
      let _debug_inject_toggle = true;

      macro_rules! no_debug {
        ($e:expr) => {{
          let _debug_inject_toggle = false;
          $e
        }}
      }

      macro_rules! inject_debug_info {
        () => {
          if debug && _debug_inject_toggle {
            let (line, col) = self.lookup.get(span.start);
            sequence.push(Rc::new(Rst::DebugCursor(DebugInfo::Location { line, col })));
          }
        }
      }
      
      // Macro for prohibiting hints/sinks before certain tokens
      macro_rules! no_flags {
        (on $b:block) => {{
          let elem = $b;
          if !matches!(next_print_flag, PrintFlag::None) {
            if let Some(flag_span) = last_print_flag_span.take() {
              self.report_error(match next_print_flag {
                PrintFlag::Hint => Problem::InvalidHintOn(elem.display_name()),
                PrintFlag::Sink => Problem::InvalidSinkOn(elem.display_name()),
                PrintFlag::None => unreachable!()
              }, &flag_span)
            }
          }
          inject_debug_info!();
          sequence.push(Rc::new(elem));
        }};
        ($b:block) => {
          if matches!(next_print_flag, PrintFlag::None) {
            $b
          } else if let Some(flag_span) = last_print_flag_span.take() {
            self.report_error(match next_print_flag {
              PrintFlag::Hint => Problem::InvalidHint,
              PrintFlag::Sink => Problem::InvalidSink,
              PrintFlag::None => unreachable!()
            }, &flag_span)
          }
        };
      }

      macro_rules! emit {
        ($elem:expr) => {{
          inject_debug_info!();
          sequence.push(Rc::new($elem));
        }}
      }

      macro_rules! emit_last_string {
        () => {{
          inject_debug_info!();
          sequence.push(Rc::new(Rst::Fragment(InternalString::from(self.reader.last_token_string()))));
        }}
      }
      
      // Shortcut macro for "unexpected token" error
      macro_rules! unexpected_token_error {
        () => {
          self.report_error(Problem::UnexpectedToken(self.reader.last_token_string().to_string()), &span)
        };
      }
      
      macro_rules! whitespace {
        (allow) => {
          if is_seq_printing {
            if let Some(ws) = pending_whitespace.take() {
              emit!(Rst::Whitespace(ws));
            }
          } else {
            pending_whitespace = None;
          }
        };
        (queue next) => {{
          if let Some((Whitespace, ..)) = self.reader.take_where(|tt| matches!(tt, Some((Whitespace, ..)))) {
            pending_whitespace = Some(self.reader.last_token_string());
          }
        }};
        (queue $ws:expr) => {
          pending_whitespace = Some($ws);
        };
        (ignore prev) => {{
          #![allow(unused_assignments)]
          pending_whitespace = None;
        }};
        (ignore next) => {
          self.reader.skip_ws();
        };
        (ignore both) => {{
          whitespace!(ignore prev);
          whitespace!(ignore next);
        }};
      }

      /// Eats as many fragments / escape sequences as possible and combines their string representations into the input `String`.
      macro_rules! consume_fragments {
        ($s:ident) => {
          while let Some((token, _)) = self.reader.take_where(|t| matches!(t, Some((Escape(..) | Fragment, ..)))) {
            match token {
              Escape(ch) => $s.push(ch),
              _ => $s.push_str(&self.reader.last_token_string()),
            }
          }
        }
      }
      
      // Parse next sequence item
      match token {
        
        // Hint
        Hint => no_flags!({
          whitespace!(allow);
          is_seq_printing = true;
          next_print_flag = PrintFlag::Hint;
          last_print_flag_span = Some(span.clone());
          continue
        }),
        
        // Sink
        Sink => no_flags!({
          // Ignore pending whitespace
          whitespace!(ignore prev);
          next_print_flag = PrintFlag::Sink;
          last_print_flag_span = Some(span.clone());
          continue
        }),

        Keyword(kw) => {
          let kwstr = kw.as_str();
          match kwstr {
            // Boolean constants
            KW_TRUE => no_debug!(no_flags!(on {
              whitespace!(ignore both);
              is_seq_printing = true;
              Rst::Boolean(true)
            })),
            KW_FALSE => no_debug!(no_flags!(on {
              whitespace!(ignore both);
              is_seq_printing = true;
              Rst::Boolean(false)
            })),
            // Control flow
            KW_RETURN | KW_CONTINUE | KW_BREAK | KW_WEIGHT => {
              whitespace!(ignore both);
              let ParsedSequence {
                sequence: charm_sequence,
                end_type: charm_end_type,
                is_text: is_charm_printing,
                extras: mut charm_extras
              } = self.parse_sequence(mode)?;
              let charm_sequence_name = charm_sequence.name.clone();
              let charm_sequence = (!charm_sequence.is_empty()).then(|| Rc::new(charm_sequence));
              match kw.as_str() {
                KW_RETURN => emit!(Rst::Return(charm_sequence)),
                KW_CONTINUE => emit!(Rst::Continue(charm_sequence)),
                KW_BREAK => emit!(Rst::Break(charm_sequence)),
                KW_WEIGHT => {
                  if mode == SequenceParseMode::BlockElement {
                    charm_extras = Some(ParsedSequenceExtras::WeightedBlockElement {
                      weight_expr: charm_sequence.unwrap_or_else(|| Rc::new(Sequence::empty(&self.info)))
                    });
                  } else {
                    self.report_error(Problem::WeightNotAllowed, &span);
                  }
                },
                _ => unreachable!()
              };
              check_dangling_printflags!();
              return Ok(ParsedSequence {
                sequence: if let Some(charm_sequence_name) = charm_sequence_name {
                  sequence.with_name(charm_sequence_name)
                } else {
                  sequence
                },
                end_type: charm_end_type,
                is_text: is_charm_printing || is_seq_printing,
                extras: charm_extras,
              })
            },
            other => self.report_error(Problem::InvalidKeyword(other.to_string()), &span),
          }          
        },
        
        // Block start
        LeftBrace => {
          // Read in the entire block
          let block = self.parse_block(false, next_print_flag)?;

          // Decide what to do with previous whitespace
          match next_print_flag {                        
            // If hinted, allow pending whitespace
            PrintFlag::Hint => {
              whitespace!(allow);
              is_seq_printing = true;
            },
            
            // If sinked, delete pending whitespace
            PrintFlag::Sink => whitespace!(ignore both),
            
            // If no flag, infer from block contents
            PrintFlag::None => {
              // Inherit hints from inner blocks
              if let Block { flag: PrintFlag::Hint, ..} = block {
                whitespace!(allow);
                is_seq_printing = true;
              }
            }
          }
          
          emit!(Rst::Block(Rc::new(block)));
        },

        // Pipe operator
        PipeOp => no_flags!({
          // Ignore pending whitespace
          whitespace!(ignore prev);
          match mode {
            SequenceParseMode::FunctionArg => {
              return Ok(ParsedSequence {
                sequence: sequence.with_name_str("argument"),
                end_type: SequenceEndType::FunctionArgEndToPipe,
                is_text: is_seq_printing,
                extras: None,
              })
            },
            SequenceParseMode::AnonFunctionExpr => {
              return Ok(ParsedSequence {
                sequence: sequence.with_name_str("anonymous function expression"),
                end_type: SequenceEndType::AnonFunctionExprToPipe,
                is_text: is_seq_printing,
                extras: None,
              })
            },
            _ => unexpected_token_error!()
          }
        }),

        // Pipe value
        PipeValue => no_flags!({
          if let Some(pipeval) = self.var_stack.get_mut(PIPE_VALUE_NAME) {
            emit!(Rst::PipeValue);
            pipeval.add_read(false);
            // Handle capturing
            if let Some((capture_frame_height, captures)) = self.capture_stack.last_mut() {
              // Variable must not exist in the current scope of the active function
              if self.var_stack.height_of(PIPE_VALUE_NAME).unwrap_or_default() < *capture_frame_height {
                captures.insert(PIPE_VALUE_NAME.into());
              }
            }
          } else {
            self.report_error(Problem::NothingToPipe, &span);
          }
        }),
        
        // Block element delimiter (when in block parsing mode)
        VertBar => no_flags!({
          // Ignore pending whitespace
          whitespace!(ignore prev);
          match mode {
            SequenceParseMode::BlockElement => {
              return Ok(ParsedSequence {
                sequence: sequence.with_name_str("block element"),
                end_type: SequenceEndType::BlockDelim,
                is_text: is_seq_printing,
                extras: None,
              })
            },
            SequenceParseMode::DynamicKey => {
              self.report_error(Problem::DynamicKeyBlockMultiElement, &span);
            },
            SequenceParseMode::FunctionBodyBlock => {
              self.report_error(Problem::FunctionBodyBlockMultiElement, &span);
            },
            _ => unexpected_token_error!()
          }
        }),
        
        // Block/func body/dynamic key end
        RightBrace => no_flags!({
          // Ignore pending whitespace
          whitespace!(ignore prev);
          match mode {
            SequenceParseMode::BlockElement => {
              return Ok(ParsedSequence {
                sequence: sequence.with_name_str("block element"),
                end_type: SequenceEndType::BlockEnd,
                is_text: true,
                extras: None,
              })
            },
            SequenceParseMode::FunctionBodyBlock => {
              return Ok(ParsedSequence {
                sequence: sequence.with_name_str("function body"),
                end_type: SequenceEndType::FunctionBodyEnd,
                is_text: true,
                extras: None,
              })
            },
            SequenceParseMode::DynamicKey => {
              return Ok(ParsedSequence {
                sequence: sequence.with_name_str("dynamic key"),
                end_type: SequenceEndType::DynamicKeyEnd,
                is_text: true,
                extras: None,
              })
            }
            _ => unexpected_token_error!()
          }
        }),
        
        // Map initializer
        At => no_flags!(on {
          match self.reader.next_solid() {
            Some((LeftParen, _)) => {
              self.parse_collection_initializer(CollectionInitKind::Map, &span)?
            },
            _ => {
              self.report_error(Problem::ExpectedToken("(".to_owned()), &self.reader.last_token_span());
              Rst::EmptyValue
            },
          }
        }),
        
        // List initializer
        LeftParen => no_flags!(on {
          self.parse_collection_initializer(CollectionInitKind::List, &span)?
        }),
        
        // Collection init termination
        RightParen => no_flags!({
          match mode {
            SequenceParseMode::CollectionInit => {
              return Ok(ParsedSequence {
                sequence,
                end_type: SequenceEndType::CollectionInitEnd,
                is_text: true,
                extras: None,
              })
            },
            _ => unexpected_token_error!()
          }
        }),
        
        // Function creation or call
        LeftBracket => {
          let func_access = self.parse_func_access(next_print_flag)?;
          
          // Handle hint/sink behavior
          match func_access {
            Rst::FuncCall(FunctionCall { flag, ..}) => {
              // If the call is not sinked, allow whitespace around it
              match flag {
                PrintFlag::Hint => {
                  is_seq_printing = true;
                  whitespace!(allow);
                },
                _ => whitespace!(ignore both)
              }
            },
            // Definitions are implicitly sinked and ignore surrounding whitespace
            Rst::FuncDef(_) => {
              whitespace!(ignore both);
            },
            // Do nothing if it's an unsupported node type, e.g. NOP
            _ => {}
          }
          
          emit!(func_access);
        },
        
        // Can be terminator for function args and anonymous function expressions
        RightBracket => no_flags!({
          match mode {
            SequenceParseMode::AnonFunctionExpr => return Ok(ParsedSequence {
              sequence: sequence.with_name_str("anonymous function expression"),
              end_type: SequenceEndType::AnonFunctionExprNoArgs,
              is_text: true,
              extras: None,
            }),
            SequenceParseMode::FunctionArg => return Ok(ParsedSequence {
              sequence: sequence.with_name_str("argument"),
              end_type: SequenceEndType::FunctionArgEndBreak,
              is_text: true,
              extras: None,
            }),
            SequenceParseMode::ParamDefaultValue => return Ok(ParsedSequence {
              sequence: sequence.with_name_str("default value"),
              end_type: SequenceEndType::ParamDefaultValueSignatureEnd,
              is_text: true,
              extras: None,
            }),
            _ => unexpected_token_error!()
          }
        }),
        
        // Variable access start
        LeftAngle => no_flags!({
          let accessors = self.parse_accessor()?;
          for accessor in accessors {
            match accessor {
              Rst::Get(..) | Rst::Depth(..) => {
                is_seq_printing = true;
                whitespace!(allow);
              },
              Rst::Set(..) | Rst::DefVar(..) | Rst::DefConst(..) => {
                // whitespace!(ignore both);
              },
              _ => unreachable!()
            }
            emit!(accessor);
          }
        }),
        
        // Variable access end
        RightAngle => no_flags!({
          match mode {
            SequenceParseMode::VariableAssignment => return Ok(ParsedSequence {
              sequence: sequence.with_name_str("setter value"),
              end_type: SequenceEndType::VariableAccessEnd,
              is_text: true,
              extras: None,
            }),
            SequenceParseMode::AccessorFallbackValue => return Ok(ParsedSequence {
              sequence: sequence.with_name_str("fallback value"),
              end_type: SequenceEndType::AccessorFallbackValueToEnd,
              is_text: true,
              extras: None,
            }),
            _ => unexpected_token_error!()
          }
        }),
        
        // These symbols are only used in special contexts and can be safely printed
        Bang | Question | Slash | Plus | Dollar | Equals | Percent
        => no_flags!(on {
          whitespace!(allow);
          is_seq_printing = true;
          let frag = self.reader.last_token_string();
          Rst::Fragment(frag)
        }),
        
        // Fragment
        Fragment => no_flags!(on {
          whitespace!(allow);
          is_seq_printing = true;
          let mut frag = self.reader.last_token_string();
          consume_fragments!(frag);
          Rst::Fragment(frag)
        }),
        
        // Whitespace (only if sequence isn't empty)
        Whitespace => no_flags!({
          // Don't set is_printing here; whitespace tokens always appear with other printing tokens
          if is_seq_printing {
            let ws = self.reader.last_token_string();
            whitespace!(queue ws);
          }
        }),
        
        // Escape sequences
        Escape(ch) => no_flags!(on {
          whitespace!(allow);
          is_seq_printing = true;
          let mut frag = InternalString::new();
          frag.push(ch);
          consume_fragments!(frag);
          Rst::Fragment(frag)
        }),
        
        // Integers
        Integer(n) => no_flags!(on {
          whitespace!(allow);
          is_seq_printing = true;
          Rst::Integer(n)
        }),
        
        // Floats
        Float(n) => no_flags!(on {
          whitespace!(allow);
          is_seq_printing = true;
          Rst::Float(n)
        }),
        
        // Empty
        EmptyValue => no_flags!(on {
          Rst::EmptyValue
        }),
        
        // Verbatim string literals
        StringLiteral(s) => no_flags!(on {
          whitespace!(allow);
          is_seq_printing = true;
          Rst::Fragment(s)
        }),
        
        // Colon can be either fragment or argument separator.
        Colon => no_flags!({
          match mode {
            SequenceParseMode::AnonFunctionExpr => return Ok(ParsedSequence {
              sequence: sequence.with_name_str("anonymous function expression"),
              end_type: SequenceEndType::AnonFunctionExprToArgs,
              is_text: true,
              extras: None,
            }),
            _ => emit_last_string!(),
          }
        }),
        
        // Semicolon can be a fragment, collection element separator, or argument separator.
        Semicolon => no_flags!({
          match mode {
            SequenceParseMode::FunctionArg => return Ok(ParsedSequence {
              sequence: sequence.with_name_str("argument"),
              end_type: SequenceEndType::FunctionArgEndNext,
              is_text: true,
              extras: None,
            }),
            SequenceParseMode::CollectionInit => return Ok(ParsedSequence {
              sequence: sequence.with_name_str("collection item"),
              end_type: SequenceEndType::CollectionInitDelim,
              is_text: true,
              extras: None,
            }),
            SequenceParseMode::VariableAssignment => return Ok(ParsedSequence {
              sequence: sequence.with_name_str("variable assignment"),
              end_type: SequenceEndType::VariableAssignDelim,
              is_text: true,
              extras: None,
            }),
            SequenceParseMode::AccessorFallbackValue => return Ok(ParsedSequence {
              sequence: sequence.with_name_str("fallback"),
              end_type: SequenceEndType::AccessorFallbackValueToDelim,
              is_text: true,
              extras: None,
            }),
            SequenceParseMode::ParamDefaultValue => return Ok(ParsedSequence {
              sequence: sequence.with_name_str("default value"),
              end_type: SequenceEndType::ParamDefaultValueSeparator,
              is_text: true,
              extras: None,
            }),
            // If we're anywhere else, just print the semicolon like normal text
            _ => emit_last_string!(),
          }
        }),
        
        // Handle unclosed string literals as hard errors
        UnterminatedStringLiteral => {
          self.report_error(Problem::UnclosedStringLiteral, &span); 
          return Err(())
        },
        _ => unexpected_token_error!(),
      }

      // If in Single Item mode, return the sequence immediately without looping
      if mode == SequenceParseMode::SingleItem {
        return Ok(ParsedSequence {
          sequence,
          end_type: SequenceEndType::SingleItemEnd,
          is_text: is_seq_printing,
          extras: None,
        })
      }
      
      // Clear flag
      next_print_flag = PrintFlag::None;
    }
    
    // Reached when the whole program has been read
    // This should only get hit for top-level sequences
    
    check_dangling_printflags!();
    
    // Return the top-level sequence
    Ok(ParsedSequence {
      sequence: sequence.with_name_str(MAIN_PROGRAM_SCOPE_NAME),
      end_type: SequenceEndType::ProgramEnd,
      is_text: is_seq_printing,
      extras: None,
    })
  }
  
  /// Parses a list/map initializer.
  fn parse_collection_initializer(&mut self, kind: CollectionInitKind, start_span: &Range<usize>) -> ParseResult<Rst> {
    match kind {
      CollectionInitKind::List => {
        self.reader.skip_ws();
        
        // Exit early on empty list
        if self.reader.eat_where(|token| matches!(token, Some((RightParen, ..)))) {
          return Ok(Rst::ListInit(Rc::new(vec![])))
        }
        
        let mut sequences = vec![];
        
        loop {
          self.reader.skip_ws();
          
          let ParsedSequence { sequence, end_type: seq_end, .. } = self.parse_sequence(SequenceParseMode::CollectionInit)?;
          
          match seq_end {
            SequenceEndType::CollectionInitDelim => {
              sequences.push(Rc::new(sequence));
            },
            SequenceEndType::CollectionInitEnd => {
              sequences.push(Rc::new(sequence));
              break
            },
            SequenceEndType::ProgramEnd => {
              self.report_error(Problem::UnclosedList, &super_range(start_span, &self.reader.last_token_span()));
              return Err(())
            },
            _ => unreachable!()
          }
        }

        // To allow trailing semicolons, remove the last element if its sequence is empty
        if let Some(seq) = sequences.last() {
          if seq.is_empty() {
            sequences.pop();
          }
        }

        Ok(Rst::ListInit(Rc::new(sequences)))
      },
      CollectionInitKind::Map => {
        let mut pairs = vec![];
        
        loop {
          let key_expr = match self.reader.next_solid() {
            // Allow blocks as dynamic keys
            Some((LeftBrace, _)) => {
              MapKeyExpr::Dynamic(Rc::new(self.parse_dynamic_expr(false)?))
            },
            // Allow fragments as keys if they are valid identifiers
            Some((Fragment, span)) => {
              let key = self.reader.last_token_string();
              if !is_valid_ident(key.as_str()) {
                self.report_error(Problem::InvalidIdentifier(key.to_string()), &span);
              }
              MapKeyExpr::Static(key)
            },
            // Allow string literals as static keys
            Some((StringLiteral(s), _)) => {
              MapKeyExpr::Static(s)
            },
            // End of map
            Some((RightParen, _)) => break,
            // Soft error on anything weird
            Some(_) => {
              self.unexpected_last_token_error();
              MapKeyExpr::Static(self.reader.last_token_string())
            },
            // Hard error on EOF
            None => {
              self.report_error(Problem::UnclosedMap, &super_range(start_span, &self.reader.last_token_span()));
              return Err(())
            }
          };
          
          self.reader.skip_ws();
          if !self.reader.eat_where(|tok| matches!(tok, Some((Equals, ..)))) {
            self.report_error(Problem::ExpectedToken("=".to_owned()), &self.reader.last_token_span());
            return Err(())
          }
          self.reader.skip_ws();
          let ParsedSequence { 
            sequence: value_expr, 
            end_type: value_expr_end, 
            .. 
          } = self.parse_sequence(SequenceParseMode::CollectionInit)?;
          
          match value_expr_end {
            SequenceEndType::CollectionInitDelim => {
              pairs.push((key_expr, Rc::new(value_expr)));
            },
            SequenceEndType::CollectionInitEnd => {
              pairs.push((key_expr, Rc::new(value_expr)));
              break
            },
            SequenceEndType::ProgramEnd => {
              self.report_error(Problem::UnclosedMap, &super_range(start_span, &self.reader.last_token_span()));
              return Err(())
            },
            _ => unreachable!()
          }
        }
        
        Ok(Rst::MapInit(Rc::new(pairs)))
      },
    }
    
  }
  
  fn parse_func_params(&mut self, start_span: &Range<usize>) -> ParseResult<Vec<(Parameter, Range<usize>)>> {
    // List of parameter names for function
    let mut params = vec![];
    // Separate set of all parameter names to check for duplicates
    let mut params_set = HashSet::new();
    // Most recently used parameter varity in this signature
    let mut last_varity = Varity::Required;
    // Keep track of whether we've encountered any variadic params
    let mut is_sig_variadic = false;
    
    // At this point there should either be ':' or ']'
    match self.reader.next_solid() {
      // ':' means there are params to be read
      Some((Colon, _)) => {
        // Read the params
        'read_params: loop {
          match self.reader.next_solid() {
            // Regular parameter
            Some((Fragment, span)) => {              
              // We only care about verifying/recording the param if it's in a valid position
              let param_name = Identifier::new(self.reader.last_token_string());
              // Make sure it's a valid identifier
              if !is_valid_ident(param_name.as_str()) {
                self.report_error(Problem::InvalidIdentifier(param_name.to_string()), &span)
              }
              // Check for duplicates
              // I'd much prefer to store references in params_set, but that's way more annoying to deal with
              if !params_set.insert(param_name.clone()) {
                self.report_error(Problem::DuplicateParameter(param_name.to_string()), &span);
              }                
              
              // Get varity of parameter
              self.reader.skip_ws();
              let (varity, full_param_span) = 
              if let Some((varity_token, varity_span)) = self.reader.take_where(|t| matches!(t, Some((Question | Star | Plus, _)))) 
              {
                (match varity_token {
                  // Optional parameter
                  Question => Varity::Optional,
                  // Optional variadic parameter
                  Star => Varity::VariadicStar,
                  // Required variadic parameter
                  Plus => Varity::VariadicPlus,
                  _ => unreachable!()
                }, super_range(&span, &varity_span))
              } else {
                (Varity::Required, span)
              };
              
              let is_param_variadic = varity.is_variadic();
                
              // Check for varity issues
              if is_sig_variadic && is_param_variadic {
                // Soft error on multiple variadics
                self.report_error(Problem::MultipleVariadicParams, &full_param_span);
              } else if !Varity::is_valid_order(last_varity, varity) {
                // Soft error on bad varity order
                self.report_error(Problem::InvalidParamOrder(last_varity.to_string(), varity.to_string()), &full_param_span);
              }

              last_varity = varity;
              is_sig_variadic |= is_param_variadic;

              // Read default value expr on optional params
              if matches!(varity, Varity::Optional) {
                let ParsedSequence {
                  sequence: default_value_seq,
                  end_type: default_value_end_type,
                  ..
                } = self.parse_sequence(SequenceParseMode::ParamDefaultValue)?;

                let should_continue = match default_value_end_type {
                  SequenceEndType::ParamDefaultValueSeparator => true,
                  SequenceEndType::ParamDefaultValueSignatureEnd => false,
                  SequenceEndType::ProgramEnd => {
                    self.report_error(Problem::UnclosedFunctionSignature, &start_span);
                    return Err(())
                  }
                  _ => unreachable!(),
                };

                let opt_param = Parameter {
                  name: param_name,
                  varity: Varity::Optional,
                  default_value_expr: (!default_value_seq.is_empty()).then(|| Rc::new(default_value_seq))
                };

                // Add parameter to list
                params.push((opt_param, full_param_span.end .. self.reader.last_token_span().start));

                // Keep reading other params if needed
                if should_continue {
                  continue 'read_params
                } else {
                  break 'read_params
                }
              }

              // Handle other varities here...

              let param = Parameter {
                name: param_name,
                varity,
                default_value_expr: None,
              };
              
              // Add parameter to list
              params.push((param, full_param_span));
                
              // Check if there are more params or if the signature is done
              match self.reader.next_solid() {
                // ';' means there are more params
                Some((Semicolon, ..)) => {
                  continue 'read_params
                },
                // ']' means end of signature
                Some((RightBracket, ..)) => {
                  break 'read_params
                },
                // Emit a hard error on anything else
                Some((_, span)) => {
                  self.report_error(Problem::UnexpectedToken(self.reader.last_token_string().to_string()), &span);
                  return Err(())
                },
                None => {
                  self.report_error(Problem::UnclosedFunctionSignature, &start_span);
                  return Err(())
                },
              }
            },
            // Error on early close
            Some((RightBracket, span)) => {
              self.report_error(Problem::MissingIdentifier, &span);
              break 'read_params
            },
            // Error on anything else
            Some((.., span)) => {
              self.report_error(Problem::InvalidIdentifier(self.reader.last_token_string().to_string()), &span)
            },
            None => {
              self.report_error(Problem::UnclosedFunctionSignature, &start_span);
              return Err(())
            }
          }
        }
      },
      // ']' means there are no params-- fall through to the next step
      Some((RightBracket, _)) => {},
      // Something weird is here, emit a hard error
      Some((.., span)) => {
        self.report_error(Problem::UnexpectedToken(self.reader.last_token_string().to_string()), &span);
        return Err(())
      },
      // Nothing is here, emit a hard error
      None => {
        self.report_error(Problem::UnclosedFunctionSignature, &start_span);
        return Err(())
      }
    }
      
    Ok(params)
  }
    
  /// Parses a function definition, anonymous function, or function call.
  fn parse_func_access(&mut self, flag: PrintFlag) -> ParseResult<Rst> {
    let start_span = self.reader.last_token_span();
    self.reader.skip_ws();
    // Check if we're defining a function (with [$|% ...]) or creating a lambda (with [? ...])
    if let Some((func_access_type_token, func_access_type_span)) 
    = self.reader.take_where(|t| matches!(t, Some((Dollar | Percent | Question, ..)))) {
      match func_access_type_token {
        // Function definition
        tt @ Dollar | tt @ Percent => {
          let is_const = matches!(tt, Percent);

          // Name of variable function will be stored in
          let (func_path, _func_path_span) = self.parse_access_path(false)?;

          // Warn user if non-variable function definition is marked as a constant
          if is_const && !func_path.is_variable() {
            self.report_warning(Problem::NestedFunctionDefMarkedConstant, &func_access_type_span);
          }
          
          self.reader.skip_ws();

          let ((body, params, end_func_sig_span), captures) = self.capture_pass(|self_| {
            // Function params
            let params = self_.parse_func_params(&start_span)?;
            let end_func_sig_span = self_.reader.last_token_span();
            
            // Read function body
            let body = self_.parse_func_body(&params, false)?;
            
            Ok((body, params, end_func_sig_span))
          })?;

          // Track variable
          if func_path.is_variable() {
            if let Some(id) = &func_path.var_name() {
              let func_def_span = super_range(&start_span, &end_func_sig_span);
              self.track_variable(id, &func_path.kind(), is_const, VarRole::Function, &func_def_span);
            }
          }
          
          Ok(Rst::FuncDef(FunctionDef {
            body: Rc::new(body.with_name_str(format!("[{}]", func_path).as_str())),
            path: Rc::new(func_path),
            params: Rc::new(params.into_iter().map(|(p, _)| p).collect()),
            capture_vars: Rc::new(captures),
            is_const,
          }))
        },
        // Lambda
        Question => {
          // Lambda params
          let params = self.parse_func_params(&start_span)?;
          self.reader.skip_ws();
          // Read function body
          let (body, captures) = self.capture_pass(|self_| self_.parse_func_body(&params, true))?;
          
          Ok(Rst::Lambda(LambdaExpr {
            capture_vars: Rc::new(captures),
            body: Rc::new(body.with_name_str("lambda")),
            params: Rc::new(params.into_iter().map(|(p, _)| p).collect()),
          }))
        },
        _ => unreachable!()
      }
    } else {
      // Function calls, both piped and otherwise
      
      // List of calls in chain. This will only contain one call if it's non-piped (chain of one).
      let mut calls: Vec<FunctionCall> = vec![];
      // Flag indicating whether call is piped (has multiple chained function calls)
      let mut is_piped = false;
      // Indicates whether the last call in the chain has been parsed
      let mut is_finished = false;
      // Indicates whether the chain has any temporal calls
      let mut is_chain_temporal = false;

      // Read all calls in chain
      while !is_finished {
        self.reader.skip_ws();
        // Argument list for current call
        let mut func_args = vec![];
        // Currently tracked temporal labels
        let mut temporal_index_labels: HashMap<InternalString, usize> = Default::default();
        // Next temporal index to be consumed
        let mut cur_temporal_index: usize = 0;
        // Anonymous call flag
        let is_anonymous = self.reader.eat_where(|t| matches!(t, Some((Bang, ..))));
        // Temporal call flag
        let mut is_temporal = false;
        // Do the user-supplied args use the pipe value?
        let mut is_pipeval_used = false;
        
        /// Reads arguments until a terminating / delimiting token is reached.
        macro_rules! parse_args {
          () => {{
            #[allow(unused_assignments)] // added because rustc whines about `spread_mode` being unused; that is a LIE
            loop {
              self.reader.skip_ws();
              let mut spread_mode = ArgumentSpreadMode::NoSpread;

              // Check for spread operators
              match self.reader.take_where(|t| matches!(t, Some((Star | Temporal | TemporalLabeled(_), ..)))) {
                // Parametric spread
                Some((Star, ..)) => {
                  self.reader.skip_ws();
                  spread_mode = ArgumentSpreadMode::Parametric;
                },
                // Unlabeled temporal spread
                Some((Temporal, ..)) => {
                  is_temporal = true;
                  self.reader.skip_ws();
                  spread_mode = ArgumentSpreadMode::Temporal { label: cur_temporal_index };
                  cur_temporal_index += 1;
                },
                // Labeled temporal spread
                Some((TemporalLabeled(label_str), ..)) => {
                  is_temporal = true;
                  self.reader.skip_ws();
                  let label_index = if let Some(label_index) = temporal_index_labels.get(&label_str) {
                    *label_index
                  } else {
                    let label_index = cur_temporal_index;
                    temporal_index_labels.insert(label_str.clone(), label_index);
                    cur_temporal_index += 1;
                    label_index
                  };
                  spread_mode = ArgumentSpreadMode::Temporal { label: label_index };
                },
                Some(_) => unreachable!(),
                None => {},
              }

              // Parse argument
              let ParsedSequence {
                sequence: arg_seq,
                end_type: arg_end,
                ..
              } = if is_piped {
                self.var_stack.push_layer();
                // Track pipe value inside arguement scope
                let pipeval_stats = VarStats {
                  writes: 1,
                  reads: 0,
                  def_span: Default::default(), // we'll never need this anyway
                  is_const: true,
                  has_fallible_read: false,
                  role: VarRole::PipeValue,
                };
                self.var_stack.define(Identifier::from(PIPE_VALUE_NAME), pipeval_stats);
                let parsed_arg_expr = self.parse_sequence_inner(SequenceParseMode::FunctionArg)?;
                is_pipeval_used |= self.var_stack.get(PIPE_VALUE_NAME).unwrap().reads > 0;
                self.analyze_top_vars();
                self.var_stack.pop_layer();
                parsed_arg_expr
              } else {
                self.parse_sequence(SequenceParseMode::FunctionArg)?
              };

              let arg = ArgumentExpr {
                expr: Rc::new(arg_seq),
                spread_mode,
              };
              func_args.push(arg);
              match arg_end {
                SequenceEndType::FunctionArgEndNext => continue,
                SequenceEndType::FunctionArgEndBreak => {
                  is_finished = true;
                  break
                },
                SequenceEndType::FunctionArgEndToPipe => {
                  is_piped = true;
                  break
                },
                SequenceEndType::ProgramEnd => {
                  self.report_error(Problem::UnclosedFunctionCall, &self.reader.last_token_span());
                  return Err(())
                },
                _ => unreachable!()
              }
            }
          }}
        }

        /// If the pipe value wasn't used, inserts it as the first argument.
        macro_rules! fallback_pipe {
          () => {
            if calls.len() > 0 && !is_pipeval_used {
              let arg = ArgumentExpr {
                expr: Rc::new(Sequence::one(Rst::PipeValue, &self.info)),
                spread_mode: ArgumentSpreadMode::NoSpread,
              };
              func_args.insert(0, arg);
            }
          }
        }
        
        self.reader.skip_ws();

        // What kind of function call is this?
        if is_anonymous {
          // Anonymous function call
          let ParsedSequence {
            sequence: func_expr,
            end_type: func_expr_end,
            ..
          } = if is_piped {
            self.var_stack.push_layer();
            // Track pipe value inside anonymous function access scope
            let pipeval_stats = VarStats {
              writes: 1,
              reads: 0,
              def_span: Default::default(), // we'll never need this anyway
              is_const: true,
              has_fallible_read: false,
              role: VarRole::PipeValue,
            };
            self.var_stack.define(Identifier::from(PIPE_VALUE_NAME), pipeval_stats);
            let seq = self.parse_sequence_inner(SequenceParseMode::AnonFunctionExpr)?;
            is_pipeval_used |= self.var_stack.get(PIPE_VALUE_NAME).unwrap().reads > 0;
            self.analyze_top_vars();
            self.var_stack.pop_layer();
            seq
          } else {
            self.parse_sequence(SequenceParseMode::AnonFunctionExpr)?
          };
          
          // Parse arguments if available
          match func_expr_end {
            // No args, fall through
            SequenceEndType::AnonFunctionExprNoArgs => {
              is_finished = true;
            },
            // Parse arguments
            SequenceEndType::AnonFunctionExprToArgs => parse_args!(),
            // Pipe without args
            SequenceEndType::AnonFunctionExprToPipe => {
              is_piped = true;
            }
            _ => unreachable!()
          }

          fallback_pipe!();
          
          // Create final node for anon function call
          let fcall = FunctionCall {
            target: FunctionCallTarget::Expression(Rc::new(func_expr)),
            arguments: Rc::new(func_args),
            flag,
            is_temporal,
          };

          calls.push(fcall);
        } else {
          // Named function call
          let (func_path, func_path_span) = self.parse_access_path(false)?;
          if let Some((token, _)) = self.reader.next_solid() {
            match token {
              // No args, fall through
              RightBracket => {
                is_finished = true;
              },
              // Parse arguments
              Colon => parse_args!(),
              // Pipe without args
              PipeOp => {
                is_piped = true;
              }
              _ => {
                self.unexpected_last_token_error();
                return Err(())
              }
            }

            fallback_pipe!();
            
            // Record access to function
            self.track_variable_access(&func_path, false, false, &func_path_span);
            
            // Create final node for function call
            let fcall = FunctionCall {
              target: FunctionCallTarget::Path(Rc::new(func_path)),
              arguments: Rc::new(func_args),
              flag,
              is_temporal,
            };

            calls.push(fcall);
          } else {
            // Found EOF instead of end of function call, emit hard error
            self.report_error(Problem::UnclosedFunctionCall, &self.reader.last_token_span());
            return Err(())
          }
        }

        is_chain_temporal |= is_temporal;
      }

      // Return the finished node
      Ok(if is_piped {
        Rst::PipedCall(PipedCall {
          flag,
          is_temporal: is_chain_temporal,
          steps: Rc::new(calls),
        })
      } else {
        Rst::FuncCall(calls.drain(..).next().unwrap())
      })
    }
  }
    
  #[inline]
  fn parse_access_path_kind(&mut self) -> AccessPathKind {    
    if let Some((token, _span)) = self.reader.take_where(
      |t| matches!(t, Some((Slash | Caret, _)))
    ) {
      match token {
        // Accessor is explicit global
        Slash => {
          AccessPathKind::ExplicitGlobal
        },
        // Accessor is for parent scope (descope operator)
        Caret => {
          let mut descope_count = 1;
          loop {
            if !self.reader.eat_where(|t| matches!(t, Some((Caret, _)))) {
              break AccessPathKind::Descope(descope_count)
            }
            descope_count += 1;
          }
        },
        _ => unreachable!()
      }
    } else {
      AccessPathKind::Local
    }
  }
  
  /// Parses an access path.
  #[inline]
  fn parse_access_path(&mut self, allow_anonymous: bool) -> ParseResult<(AccessPath, Range<usize>)> {
    self.reader.skip_ws();
    let mut idparts = vec![];
    let start_span = self.reader.last_token_span();
    let mut access_kind = AccessPathKind::Local;

    if allow_anonymous && self.reader.eat_where(|t| matches!(t, Some((Bang, ..)))) {
      self.reader.skip_ws();
      let ParsedSequence {
        sequence: anon_expr,
        end_type: anon_end_type,
        ..
      } = self.parse_sequence(SequenceParseMode::SingleItem)?;
      match anon_end_type {
        SequenceEndType::SingleItemEnd => {
          idparts.push(AccessPathComponent::AnonymousValue(Rc::new(anon_expr)));
        },
        SequenceEndType::ProgramEnd => {
          self.report_error(Problem::UnclosedVariableAccess, &self.reader.last_token_span());
          return Err(())
        },
        _ => unreachable!(),
      }
    } else {
      // Check for global/descope specifiers
      access_kind = self.parse_access_path_kind();
      
      let first_part = self.reader.next_solid();
      
      // Parse the first part of the path
      match first_part {
        // The first part of the path may only be a variable name (for now)
        Some((Fragment, span)) => {
          let varname = Identifier::new(self.reader.last_token_string());
          if is_valid_ident(varname.as_str()) {
            idparts.push(AccessPathComponent::Name(varname));
          } else {
            self.report_error(Problem::InvalidIdentifier(varname.to_string()), &span);
          }
        },
        // An expression can also be used to provide the variable
        Some((LeftBrace, _)) => {
          let dynamic_key_expr = self.parse_dynamic_expr(false)?;
          idparts.push(AccessPathComponent::DynamicKey(Rc::new(dynamic_key_expr)));
        },
        // TODO: Check for dynamic slices here too!
        // First path part can't be a slice
        Some((Colon, span)) => {
          self.reader.take_where(|t| matches!(t, Some((Integer(_), ..))));
          self.report_error(Problem::AccessPathStartsWithSlice, &super_range(&span, &self.reader.last_token_span()));
        }
        // Prevent other slice forms
        Some((Integer(_), span)) => {
          self.reader.skip_ws();
          if self.reader.eat_where(|t| matches!(t, Some((Colon, ..)))) {
            self.report_error(Problem::AccessPathStartsWithSlice, &super_range(&span, &self.reader.last_token_span()));
          } else {
            self.report_error(Problem::AccessPathStartsWithIndex, &span);
          }
        },
        Some((.., span)) => {
          self.report_error(Problem::InvalidIdentifier(self.reader.last_token_string().to_string()), &span);
        },
        None => {
          self.report_error(Problem::MissingIdentifier, &start_span);
          return Err(())
        }
      }
    }
    
    // Parse the rest of the path
    loop {
      // We expect a '/' between each component, so check for that first.
      // If it's anything else, terminate the path and return it.
      self.reader.skip_ws();
      if self.reader.eat_where(|t| matches!(t, Some((Slash, ..)))) {
        // From here we expect to see either another key (fragment) or index (integer).
        // If it's anything else, return a syntax error.
        let component = self.reader.next_solid();
        match component {
          // Key
          Some((Fragment, span)) => {
            let varname = Identifier::new(self.reader.last_token_string());
            if is_valid_ident(varname.as_str()) {
              idparts.push(AccessPathComponent::Name(varname));
            } else {
              self.report_error(Problem::InvalidIdentifier(varname.to_string()), &span);
            }
          },
          // Index or slice with static from-bound
          Some((Integer(i), _)) => {
            self.reader.skip_ws();
            // Look for a colon to see if it's a slice
            if self.reader.eat_where(|t| matches!(t, Some((Colon, ..)))) {
              self.reader.skip_ws();
              match self.reader.peek() {
                // Between-slice with static from- + to-bounds
                Some((Integer(j), ..)) => {
                  let j = *j;
                  self.reader.skip_one();
                  idparts.push(AccessPathComponent::Slice(SliceExpr::Between(SliceIndex::Static(i), SliceIndex::Static(j))));
                },
                // Between-slice with static from-bound + dynamic to-bound
                Some((LeftBrace, ..)) => {
                  let to_expr = Rc::new(self.parse_dynamic_expr(true)?);
                  idparts.push(AccessPathComponent::Slice(SliceExpr::Between(SliceIndex::Static(i), SliceIndex::Dynamic(to_expr))));
                },
                // From-slice with static from-bound
                Some((Slash | RightAngle | Equals | Question | Semicolon, ..)) => {
                  idparts.push(AccessPathComponent::Slice(SliceExpr::From(SliceIndex::Static(i))));
                },
                // Found something weird as the to-bound, emit an error
                Some(_) => {
                  self.reader.next();
                  let token = self.reader.last_token_string().to_string();
                  self.report_error(Problem::InvalidSliceBound(token), &self.reader.last_token_span());
                },
                None => {
                  self.report_error(Problem::UnclosedVariableAccess, &super_range(&start_span, &self.reader.last_token_span()));
                  return Err(())
                }
              }
            } else {
              // No colon, so it's an index
              idparts.push(AccessPathComponent::Index(i));
            }
          },
          // Full- or to-slice
          Some((Colon, _)) => {
            self.reader.skip_ws();
            match self.reader.peek() {
              // To-slice with static bound
              Some((Integer(to), ..)) => {
                let to = *to;
                self.reader.skip_one();
                idparts.push(AccessPathComponent::Slice(SliceExpr::To(SliceIndex::Static(to))));
              },
              // To-slice with dynamic bound
              Some((LeftBrace, ..)) => {
                let to_expr = Rc::new(self.parse_dynamic_expr(true)?);
                idparts.push(AccessPathComponent::Slice(SliceExpr::To(SliceIndex::Dynamic(to_expr))));
              },
              // Full-slice
              Some((Slash | RightAngle | Equals | Question | Semicolon, ..)) => {
                idparts.push(AccessPathComponent::Slice(SliceExpr::Full));
              },
              // Found something weird as the to-bound, emit an error
              Some(_) => {
                self.reader.next();
                let token = self.reader.last_token_string().to_string();
                self.report_error(Problem::InvalidSliceBound(token), &self.reader.last_token_span());
              },
              None => {
                self.report_error(Problem::UnclosedVariableAccess, &super_range(&start_span, &self.reader.last_token_span()));
                return Err(())
              }
            }
          },
          // Dynamic key or slice with dynamic from-bound
          Some((LeftBrace, _)) => {
            let expr = Rc::new(self.parse_dynamic_expr(false)?);
            self.reader.skip_ws();
            // Look for a colon to see if it's a slice
            if self.reader.eat_where(|t| matches!(t, Some((Colon, ..)))) {
              self.reader.skip_ws();
              match self.reader.peek() {
                // Between-slice with a dynamic from-bound + static to-bound
                Some((Integer(to), ..)) => {
                  let to = *to;
                  self.reader.skip_one();
                  idparts.push(AccessPathComponent::Slice(SliceExpr::Between(SliceIndex::Dynamic(expr), SliceIndex::Static(to))));
                },
                // Between-slice with dynamic from- + to-bounds
                Some((LeftBrace, ..)) => {
                  let to_expr = Rc::new(self.parse_dynamic_expr(true)?);
                  idparts.push(AccessPathComponent::Slice(SliceExpr::Between(SliceIndex::Dynamic(expr), SliceIndex::Dynamic(to_expr))));
                },
                // From-slice with dynamic bound
                Some((Slash | RightAngle | Equals | Question | Semicolon, ..)) => {
                  idparts.push(AccessPathComponent::Slice(SliceExpr::From(SliceIndex::Dynamic(expr))));
                },
                // Found something weird as the to-bound, emit an error
                Some(_) => {
                  self.reader.next();
                  let token = self.reader.last_token_string().to_string();
                  self.report_error(Problem::InvalidSliceBound(token), &self.reader.last_token_span());
                },
                None => {
                  self.report_error(Problem::UnclosedVariableAccess, &super_range(&start_span, &self.reader.last_token_span()));
                  return Err(())
                }
              }
            } else {
              // No colon, so it's an dynamic key
              idparts.push(AccessPathComponent::DynamicKey(expr));
            }
          },
          Some((.., span)) => {
            // error
            self.report_error(Problem::InvalidIdentifier(self.reader.last_token_string().to_string()), &span);
          },
          None => {
            self.report_error(Problem::MissingIdentifier, &self.reader.last_token_span());
            return Err(())
          }
        }
      } else {
        return Ok((AccessPath::new(idparts, access_kind), start_span.start .. self.reader.last_token_span().start))
      }
    }
  }
    
  /// Parses a dynamic expression (a linear block).
  fn parse_dynamic_expr(&mut self, expect_opening_brace: bool) -> ParseResult<Sequence> {
    if expect_opening_brace && !self.reader.eat_where(|t| matches!(t, Some((LeftBrace, _)))) {
      self.report_error(Problem::ExpectedToken("{".to_owned()), &self.reader.last_token_span());
      return Err(())
    }
    
    let start_span = self.reader.last_token_span();
    let ParsedSequence { sequence, end_type, .. } = self.parse_sequence(SequenceParseMode::DynamicKey)?;
    
    match end_type {
      SequenceEndType::DynamicKeyEnd => {},
      SequenceEndType::ProgramEnd => {
        // Hard error if block isn't closed
        let err_span = start_span.start .. self.source.len();
        self.report_error(Problem::UnclosedBlock, &err_span);
        return Err(())
      },
      _ => unreachable!()
    }
    
    Ok(sequence)
  }

  /// Parses a function body and DOES NOT capture variables.
  fn parse_func_body(&mut self, params: &Vec<(Parameter, Range<usize>)>, allow_inline: bool) -> ParseResult<Sequence> {
    self.reader.skip_ws();

    let is_block_body = if allow_inline {
      // Determine if the body is a block and eat the opening brace if available
      self.reader.eat_where(|t| matches!(t, Some((LeftBrace, _))))
    } else {
      if !self.reader.eat_where(|t| matches!(t, Some((LeftBrace, _)))) {
        self.report_error(Problem::ExpectedToken("{".to_owned()), &self.reader.last_token_span());
        return Err(())
      }
      true
    };

    let start_span = self.reader.last_token_span();

    // Define each parameter as a variable in the current var_stack frame so they are not accidentally captured
    for (param, span) in params {
      self.var_stack.define(param.name.clone(), VarStats {
        reads: 0,
        writes: 1,
        def_span: span.clone(),
        is_const: true,
        has_fallible_read: false,
        role: if param.is_optional() && param.default_value_expr.is_none() {
          VarRole::FallibleOptionalArgument
        } else { 
          VarRole::Argument 
        }
      });
    }

    // parse_sequence_inner() is used here so that the new stack frame can be customized before use
    let ParsedSequence { sequence, end_type, .. } = self.parse_sequence_inner(if is_block_body {
      SequenceParseMode::FunctionBodyBlock
    } else {
      SequenceParseMode::SingleItem
    })?;

    match end_type {
      SequenceEndType::FunctionBodyEnd | SequenceEndType::SingleItemEnd => {},
      SequenceEndType::ProgramEnd => {
        let err_span = start_span.start .. self.source.len();
        self.report_error(if is_block_body { 
          Problem::UnclosedFunctionBody 
        } else { 
          Problem::MissingFunctionBody 
        }, &err_span);
        return Err(())
      },
      _ => unreachable!()
    }

    Ok(sequence)
  }
  
  fn capture_pass<T>(&mut self, parse_func: impl FnOnce(&mut Self) -> ParseResult<T>) -> ParseResult<(T, Vec<Identifier>)> {
    // Since we're about to push another var_stack frame, we can use the current depth of var_stack as the index
    let capture_height = self.var_stack.depth();

    // Push a new capture frame
    self.capture_stack.push((capture_height, Default::default()));

    // Push a new variable frame
    self.var_stack.push_layer();

    // Call parse_func
    let parse_out = parse_func(self)?;

    // Run static analysis on variable/param usage
    self.analyze_top_vars();

    self.var_stack.pop_layer();

    // Pop the topmost capture frame and grab the set of captures
    let (_, mut capture_set) = self.capture_stack.pop().unwrap();

    Ok((parse_out, capture_set.drain().collect()))
  }
    
  /// Parses a block.
  fn parse_block(&mut self, expect_opening_brace: bool, flag: PrintFlag) -> ParseResult<Block> {
    if expect_opening_brace && !self.reader.eat_where(|t| matches!(t, Some((LeftBrace, _)))) {
      self.report_error(Problem::ExpectedToken("{".to_owned()), &self.reader.last_token_span());
      return Err(())
    }
    
    // Get position of starting brace for error reporting
    let start_pos = self.reader.last_token_pos();
    // Keeps track of inherited hinting
    let mut auto_hint = false;
    // Is the block weighted?
    let mut is_weighted = false;
    // Block content
    let mut elements = vec![];
    
    loop {
      let ParsedSequence { 
        sequence, 
        end_type, 
        is_text, 
        extras 
      } = self.parse_sequence(SequenceParseMode::BlockElement)?;
      
      auto_hint |= is_text;

      let element = BlockElement {
        main: Rc::new(sequence),
        weight: if let Some(ParsedSequenceExtras::WeightedBlockElement { weight_expr }) = extras {
          is_weighted = true;
          // Optimize constant weights
          Some(match (weight_expr.len(), weight_expr.first().map(Rc::as_ref)) {
            (1, Some(Rst::Integer(n))) => BlockWeight::Constant(*n as f64),
            (1, Some(Rst::Float(n))) => BlockWeight::Constant(*n),
            _ => BlockWeight::Dynamic(weight_expr)
          })
        } else {
          None
        },
      };
      
      match end_type {
        SequenceEndType::BlockDelim => {
          elements.push(element);
        },
        SequenceEndType::BlockEnd => {
          elements.push(element);
          break
        },
        SequenceEndType::ProgramEnd => {
          // Hard error if block isn't closed
          let err_span = start_pos .. self.source.len();
          self.report_error(Problem::UnclosedBlock, &err_span);
          return Err(())
        },
        _ => unreachable!()
      }
    }
    
    // Figure out the printflag before returning the block
    if auto_hint && flag != PrintFlag::Sink {
      Ok(Block::new(PrintFlag::Hint, is_weighted, elements))
    } else {
      Ok(Block::new(flag, is_weighted, elements))
    }
  }
  
  /// Parses an identifier.
  fn parse_ident(&mut self) -> ParseResult<Identifier> {
    if let Some((token, span)) = self.reader.next_solid() {
      match token {
        Fragment => {
          let idstr = self.reader.last_token_string();
          if !is_valid_ident(idstr.as_str()) {
            self.report_error(Problem::InvalidIdentifier(idstr.to_string()), &span);
          }
          Ok(Identifier::new(idstr))
        },
        _ => {
          self.unexpected_last_token_error();
          Err(())
        }
      }
    } else {
      self.report_error(Problem::MissingIdentifier, &self.reader.last_token_span());
      Err(())
    }
  }

  #[inline]
  fn track_variable(&mut self, id: &Identifier, access_kind: &AccessPathKind, is_const: bool, role: VarRole, def_span: &Range<usize>) {
    // Check if there's already a variable with this name
    let (prev_tracker, requested_depth, found_depth) = match access_kind {        
      AccessPathKind::Local => {
        (self.var_stack.get(id), 0, self.var_stack.depth_of(id))
      },
      AccessPathKind::Descope(n) => {
        let (v, d) = self.var_stack
          .get_parent_depth(id, *n)
          .map(|(v, d)| (Some(v), Some(d)))
          .unwrap_or_default();
        (v, *n, d)
      },
      AccessPathKind::ExplicitGlobal => {
        let rd = self.var_stack.depth();
        let (v, d) = self.var_stack
          .get_parent_depth(id, rd)
          .map(|(v, d)| (Some(v), Some(d)))
          .unwrap_or_default();
        (v, rd, d)
      },
    };

    // Check for constant redef
    if let Some(prev_tracker) = prev_tracker {
      if prev_tracker.is_const && found_depth == Some(requested_depth) {
        self.report_error(Problem::ConstantRedefinition(id.to_string()), def_span);
      }
    }

    // Create variable tracking info
    let v = VarStats {
      writes: 0,
      reads: 0,
      def_span: def_span.clone(),
      has_fallible_read: false,
      is_const,
      role,
    };

    // Add to stack
    match access_kind {
      AccessPathKind::Local => {
        self.var_stack.define(id.clone(), v);
      },
      AccessPathKind::Descope(n) => {
        self.var_stack.define_parent(id.clone(), v, *n);
      },
      AccessPathKind::ExplicitGlobal => {
        self.var_stack.define_parent(id.clone(), v, self.var_stack.depth());
      },
    }
  }

  #[inline]
  fn track_variable_access(&mut self, path: &AccessPath, is_write: bool, fallback_hint: bool, span: &Range<usize>) {
    // Handle access stats
    if let Some(id) = &path.var_name() {
      let tracker = match path.kind() {
        AccessPathKind::Local => {
          self.var_stack.get_mut(id)
        },
        AccessPathKind::Descope(n) => {
          self.var_stack.get_parent_mut(id, n)
        },
        AccessPathKind::ExplicitGlobal => {
          self.var_stack.get_parent_mut(id, self.var_stack.depth())
        }
      };

      // Update tracker
      if let Some(tracker) = tracker {
        if is_write {
          tracker.writes += 1;

          if tracker.is_const {
            self.report_error(Problem::ConstantReassignment(id.to_string()), span);
          }
        } else {
          tracker.add_read(!fallback_hint);

          // Warn the user if they're accessing a fallible optional argument without a fallback
          if tracker.has_fallible_read && tracker.role == VarRole::FallibleOptionalArgument {
            self.report_warning(Problem::FallibleOptionalArgAccess(id.to_string()), span);
          }
        }
      }
    }
    
    // Handle captures
    if path.kind().is_local() {
      // At least one capture frame must exist
      if let Some((capture_frame_height, captures)) = self.capture_stack.last_mut() {
        // Must be accessing a variable
        if let Some(id) = path.var_name() {
          // Variable must not exist in the current scope of the active function
          if self.var_stack.height_of(&id).unwrap_or_default() < *capture_frame_height {
            captures.insert(id);
          }
        }
      }
    }
  }

  #[inline]
  fn analyze_top_vars(&mut self) {
    let mut unused_vars: Vec<(String, VarRole, Range<usize>)> = vec![];

    // Can't warn inside the loop due to bOrRoWiNg RuLeS!
    // Have to store the warning contents in a vec first...
    for (id, tracker) in self.var_stack.iter_top() {
      if tracker.reads == 0 {
        unused_vars.push((id.to_string(), tracker.role, tracker.def_span.clone()));
      }
    }

    // Generate warnings
    unused_vars.sort_by(|(.., a_span), (.., b_span)| a_span.start.cmp(&b_span.start));
    for (name, role, span) in unused_vars {
      match role {
        VarRole::Normal => self.report_warning(Problem::UnusedVariable(name), &span),
        VarRole::Argument => self.report_warning(Problem::UnusedParameter(name), &span),
        VarRole::Function => self.report_warning(Problem::UnusedFunction(name), &span),
        // Ignore any other roles
        _ => {},
      }
    }
  }
    
  /// Parses one or more accessors (getter/setter/definition).
  #[inline(always)]
  fn parse_accessor(&mut self) -> ParseResult<Vec<Rst>> {
    let mut accessors = vec![];

    macro_rules! add_accessor {
      ($rst:expr) => {{
        let rst = $rst;
        accessors.push(rst);
      }}
    }
    
    'read: loop {      
      self.reader.skip_ws();

      // Check if the accessor ends here as long as there's at least one component
      if !accessors.is_empty() && self.reader.eat_where(|t| matches!(t, Some((RightAngle, ..)))) {
        break
      }
      
      let (is_def, is_const_def) = if let Some((def_token, ..)) 
      = self.reader.take_where(|t| matches!(t, Some((Dollar | Percent, ..)))) {
        match def_token {
          // Variable declaration
          Dollar => (true, false),
          // Constant declaration
          Percent => (true, true),
          _ => unreachable!()
        }
      } else {
        (false, false)
      };

      let access_start_span = self.reader.last_token_span();

      self.reader.skip_ws();
      
      // Check if it's a definition. If not, it's a getter or setter
      if is_def {
        // Check for accessor modifiers
        let access_kind = self.parse_access_path_kind();
        self.reader.skip_ws();
        // Read name of variable we're defining
        let var_name = self.parse_ident()?;

        let def_span = access_start_span.start .. self.reader.last_token_span().end;
        
        if let Some((token, _token_span)) = self.reader.next_solid() {
          match token {
            // Empty definition
            RightAngle => {              
              if is_const_def {
                self.track_variable(&var_name, &access_kind, true, VarRole::Normal, &def_span);
                add_accessor!(Rst::DefConst(var_name, access_kind, None));
              } else {
                self.track_variable(&var_name, &access_kind, false, VarRole::Normal, &def_span);
                add_accessor!(Rst::DefVar(var_name, access_kind, None));
              }
              break 'read
            },
            // Accessor delimiter
            Semicolon => {
              if is_const_def {
                self.track_variable(&var_name, &access_kind, true, VarRole::Normal, &def_span);
                add_accessor!(Rst::DefConst(var_name, access_kind, None));
              } else {
                self.track_variable(&var_name, &access_kind, false, VarRole::Normal, &def_span);
                add_accessor!(Rst::DefVar(var_name, access_kind, None));
              }
              continue 'read;
            },
            // Definition and assignment
            Equals => {
              self.reader.skip_ws();
              let ParsedSequence { 
                sequence: setter_expr, 
                end_type: setter_end_type, 
                .. 
              } = self.parse_sequence(SequenceParseMode::VariableAssignment)?;

              let def_span = access_start_span.start .. self.reader.last_token_span().start;
              if is_const_def {
                self.track_variable(&var_name, &access_kind, true, VarRole::Normal, &def_span);
                add_accessor!(Rst::DefConst(var_name, access_kind, Some(Rc::new(setter_expr))));
              } else {
                self.track_variable(&var_name, &access_kind, false, VarRole::Normal, &def_span);
                add_accessor!(Rst::DefVar(var_name, access_kind, Some(Rc::new(setter_expr))));
              }
              
              match setter_end_type {
                SequenceEndType::VariableAssignDelim => {
                  continue 'read
                },
                SequenceEndType::VariableAccessEnd => {
                  break 'read
                },
                SequenceEndType::ProgramEnd => {
                  self.report_error(Problem::UnclosedVariableAccess, &self.reader.last_token_span());
                  return Err(())
                },
                _ => unreachable!()
              }
            },
            // Ran into something we don't support
            _ => {
              self.unexpected_last_token_error();
              return Err(())
            }
          }
        } else {
          self.report_error(Problem::UnclosedVariableAccess, &self.reader.last_token_span());
          return Err(())
        }
      } else {
        // Read the path to what we're accessing
        let mut is_depth_op = false;
        let (var_path, var_path_span) = self.parse_access_path(true)?;
        
        self.reader.skip_ws();

        // Check for depth operator
        if let Some((_, depth_op_range)) = self.reader.take_where(|t| matches!(t, Some((And, _)))) {
          if var_path.is_variable() && var_path.var_name().is_some() {
            is_depth_op = true;
          } else if var_path.len() == 1 && matches!(var_path.first(), Some(AccessPathComponent::DynamicKey(..))) {
            self.report_error(Problem::DynamicDepth, &depth_op_range);
          } else {
            self.report_error(Problem::InvalidDepthUsage, &depth_op_range);
          }
        }
        
        if let Some((token, cur_token_span)) = self.reader.next_solid() {
          match token {
            // If we hit a '>', it's a getter
            RightAngle => {
              self.track_variable_access(&var_path, false, false, &var_path_span);
              add_accessor!(if is_depth_op {
                Rst::Depth(var_path.var_name().unwrap(), var_path.kind(), None)
              } else { 
                Rst::Get(Rc::new(var_path), None)
              });
              break 'read;
            },
            // If we hit a ';', it's a getter with another accessor chained after it
            Semicolon => {
              self.track_variable_access(&var_path, false, false, &var_path_span);
              add_accessor!(if is_depth_op {
                Rst::Depth(var_path.var_name().unwrap(), var_path.kind(), None)
              } else { 
                Rst::Get(Rc::new(var_path), None)
              });
              continue 'read;
            },
            // If we hit a `?`, it's a getter with a fallback
            Question => {
              self.reader.skip_ws();
              let ParsedSequence {
                sequence: fallback_expr,
                end_type: fallback_end_type,
                ..
              } = self.parse_sequence(SequenceParseMode::AccessorFallbackValue)?;

              self.track_variable_access(&var_path, false, true, &var_path_span);

              add_accessor!(if is_depth_op {
                Rst::Depth(var_path.var_name().unwrap(), var_path.kind(), Some(Rc::new(fallback_expr)))
              } else { 
                Rst::Get(Rc::new(var_path), Some(Rc::new(fallback_expr)))
              });

              match fallback_end_type {
                SequenceEndType::AccessorFallbackValueToDelim => continue 'read,
                SequenceEndType::AccessorFallbackValueToEnd => break 'read,
                // Error
                SequenceEndType::ProgramEnd => {
                  self.report_error(Problem::UnclosedVariableAccess, &cur_token_span);
                  return Err(())
                },
                _ => unreachable!()
              }
            },
            // If we hit a '=' here, it's a setter
            Equals => {
              self.reader.skip_ws();
              let ParsedSequence {
                sequence: setter_rhs_expr,
                end_type: setter_rhs_end,
                ..
              } = self.parse_sequence(SequenceParseMode::VariableAssignment)?;
              let assign_end_span = self.reader.last_token_span();
              let setter_span = super_range(&access_start_span, &assign_end_span);
              // Don't allow setters directly on anonymous values
              if var_path.is_anonymous() && var_path.len() == 1 {
                self.report_error(Problem::AnonValueAssignment, &setter_span);
              }

              self.track_variable_access(&var_path, true, false, &setter_span);
              add_accessor!(Rst::Set(Rc::new(var_path), Rc::new(setter_rhs_expr)));

              // Assignment is not valid if we're using depth operator
              if is_depth_op {
                self.report_error(Problem::DepthAssignment, &(cur_token_span.start .. assign_end_span.start));
              }

              match setter_rhs_end {
                // Accessor was terminated
                SequenceEndType::VariableAccessEnd => {                  
                  break 'read;
                },
                // Expression ended with delimiter
                SequenceEndType::VariableAssignDelim => {
                  continue 'read;
                },
                // Error
                SequenceEndType::ProgramEnd => {
                  self.report_error(Problem::UnclosedVariableAccess, &self.reader.last_token_span());
                  return Err(())
                },
                _ => unreachable!()
              }
            },
            // Anything else is an error
            _ => {
              self.unexpected_last_token_error();
              return Err(())
            }
          }
        } else {
          self.report_error(Problem::UnclosedVariableAccess, &self.reader.last_token_span());
          return Err(())
        }
      }
    }
    
    Ok(accessors)
  }
}