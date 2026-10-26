"""Structural analysis of the real Qwen Q8 OpenCL matrix kernels.

This is a compiler development stage, not an offload authorization. No plan
from this module is dispatchable until numerical semantics, weight admission,
runtime storage checks and the masked executor are implemented together.
"""
from functools import lru_cache
import hashlib
from pathlib import Path
import re
import subprocess

from kernel_compiler import ast, canonical_type, MAX_SOURCE

ROOT = Path(__file__).resolve().parent / 'q8-reference'
REFERENCES = {'mm': 'mul_mm_q8_0_f32_l4_lm.cl', 'mv': 'mul_mv_q8_0_f32_flat.cl'}
STRICT = ('-cl-std=CL3.0',)
FAST = STRICT + ('-cl-mad-enable', '-cl-unsafe-math-optimizations',
                 '-cl-finite-math-only', '-cl-fast-relaxed-math')
CAPABILITIES = ('-Dcl_intel_subgroups=1', '-Dcl_intel_required_subgroup_size=1')
BUILTINS = {'get_global_id', 'get_group_id', 'get_local_id', 'get_local_size',
            'get_sub_group_id', 'get_sub_group_local_id', 'sub_group_reduce_add',
            'convert_float4', 'convert_float8', 'vload8', 'barrier', 'mad'}
KINDS = {'FunctionDecl', 'ParmVarDecl', 'VarDecl', 'CompoundStmt', 'DeclStmt',
         'BinaryOperator', 'UnaryOperator', 'DeclRefExpr', 'ImplicitCastExpr',
         'CStyleCastExpr', 'CallExpr', 'ForStmt', 'IfStmt', 'ArraySubscriptExpr',
         'IntegerLiteral', 'FloatingLiteral', 'CompoundAssignOperator',
         'ExtVectorElementExpr', 'CompoundLiteralExpr', 'InitListExpr',
         'UnaryExprOrTypeTraitExpr', 'ParenExpr', 'OpenCLKernelAttr',
         'OpenCLIntelReqdSubGroupSizeAttr'}
def without_comments(source):
    # Preserve newlines and strings while replacing comments by whitespace,
    # matching C preprocessing's token separation (a/**/b is not ab).
    tokens = re.compile(r'//[^\n]*|/\*[\s\S]*?\*/|"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])*\'')
    return tokens.sub(lambda m: ''.join('\n' if c == '\n' else ' ' for c in m[0])
                      if m[0].startswith(('/',)) else m[0], source)


@lru_cache(maxsize=1)
def audited_directives():
    return frozenset(' '.join(line.split()) for filename in REFERENCES.values()
                     for line in without_comments((ROOT / filename).read_text()).splitlines()
                     if line.lstrip().startswith('#'))


def source_allowed(source):
    # Clang JSON omits some pragma state. Only audited reference directives
    # are admitted. Strip comments before recognizing directives, and refuse
    # alternate directive spellings, escapes, macro token pasting/stringizing
    # and pragma operators. This intentionally declines unfamiliar source;
    # it is not a general C preprocessor.
    source = without_comments(source)
    if any(token in source for token in ('_Pragma', '__pragma', '\\', '%:', '??')):
        return False
    for line in source.splitlines():
        if not line.lstrip().startswith('#'):
            if '#' in line:
                return False
            continue
        if ' '.join(line.split()) not in audited_directives():
            return False
    return True


def source_functions(tree):
    result = []
    for node in tree.get('inner', []):
        if node.get('kind') == 'VarDecl':
            raise ValueError('global storage is not supported')
        if node.get('kind') != 'FunctionDecl':
            continue
        if 'includedFrom' in node.get('loc', {}):
            # Only the compiler's default header is available: includes and
            # source-location directives are refused before invoking Clang.
            continue
        if not any(n.get('kind') == 'OpenCLKernelAttr' for n in node.get('inner', [])):
            raise ValueError('source function declarations/definitions are not imported builtins')
        result.append(node)
    return result


def normalize(function, source):
    symbols = {}
    encoded = source.encode()

    def source_range(node, last_only=False):
        span = node['range']
        begin, end = span['begin'], span['end']
        begin = begin.get('spellingLoc', begin)
        end = end.get('spellingLoc', end)
        if any('includedFrom' in p or 'file' in p for p in (begin, end)):
            raise ValueError('attribute/selector spelling outside the kernel source')
        start = end['offset'] if last_only else begin['offset']
        stop = end['offset'] + end['tokLen']
        if not 0 <= start < stop <= len(encoded):
            raise ValueError('invalid source span')
        return encoded[start:stop].decode('ascii')

    def typed(value):
        if isinstance(value, dict):
            if 'qualType' in value:
                return {'canonicalType': canonical_type({'type': value})}
            return {k: typed(v) for k, v in value.items()}
        if isinstance(value, list):
            return [typed(v) for v in value]
        return value

    def visit(node):
        if not node:
            return {}
        kind = node['kind']
        if kind not in KINDS:
            raise ValueError('unsupported AST operation')
        if kind == 'ParenExpr':
            return visit(node['inner'][0])
        if kind == 'OpenCLKernelAttr':
            return {'kind': kind}
        if kind == 'OpenCLIntelReqdSubGroupSizeAttr':
            text = source_range(node)
            size = re.fullmatch(r'intel_reqd_sub_group_size\s*\(\s*(\d+)\s*\)', text)
            if not size:
                raise ValueError('subgroup attribute needs an explicit literal')
            return {'kind': kind, 'size': int(size[1])}
        if kind in ('ParmVarDecl', 'VarDecl'):
            symbols[node['id']] = len(symbols)
        ignored = {'id', 'loc', 'range', 'inner', 'referencedDecl'}
        if kind in ('FunctionDecl', 'ParmVarDecl', 'VarDecl'):
            ignored |= {'name', 'mangledName'}
        result = {k: typed(v) for k, v in node.items() if k not in ignored}
        if kind == 'ExtVectorElementExpr':
            # JSON AST does not encode the selected lanes; ignoring the source
            # token would incorrectly equate .s0 with .s1 and authorize bad math.
            selector = source_range(node, last_only=True)
            if not re.fullmatch(r'(?:s[0-9a-fA-F]+|[xyzw]+|lo|hi|even|odd)', selector):
                raise ValueError('unsupported vector selector')
            result['selector'] = selector
        if kind == 'DeclRefExpr':
            decl = node['referencedDecl']
            if decl['kind'] == 'FunctionDecl':
                if decl['name'] not in BUILTINS:
                    raise ValueError('unsupported imported call')
                result['reference'] = [decl['kind'], decl['name'], canonical_type(decl)]
            else:
                if decl['id'] not in symbols:
                    raise ValueError('reference outside kernel')
                result['reference'] = [decl['kind'], symbols[decl['id']], canonical_type(decl)]
        if 'inner' in node:
            result['inner'] = [visit(child) for child in node['inner']]
        return result

    return visit(function)


@lru_cache(maxsize=8)
def reference(variant, flags, clang):
    source = (ROOT / REFERENCES[variant]).read_text()
    tree = ast(source, clang, flags + CAPABILITIES)
    functions = source_functions(tree)
    assert len(functions) == 1
    return normalize(functions[0], source), hashlib.sha256(source.encode()).hexdigest()


def analyze(source, options='-cl-std=CL3.0', clang='/usr/bin/clang-18'):
    if not isinstance(source, str) or not isinstance(options, str):
        return {}
    if len(source.encode()) > MAX_SOURCE or '\0' in source or len(options) > 4096:
        return {}
    options = tuple(options.split())
    if len(options) != len(set(options)) or not source_allowed(source):
        return {}
    flags = next((profile for profile in (STRICT, FAST) if set(options) == set(profile)), None)
    if flags is None:
        return {}
    try:
        functions = source_functions(ast(source, clang, flags + CAPABILITIES))
        plans = {}
        for function in functions:
            normalized = normalize(function, source)
            for variant in REFERENCES:
                pattern, digest = reference(variant, flags, clang)
                if normalized == pattern:
                    plans[function['name']] = {
                        'schema': 'shield-opencl-q8-analysis/1', 'dispatchable': False,
                        'operation': 'q8_0-f32-matmul', 'variant': variant,
                        'referenceSha256': digest, 'buildOptions': list(flags),
                        'arguments': {'quants': 0, 'scales': 1, 'input': 2, 'inputOffset': 3,
                                      'output': 4, 'outputOffset': 5},
                        'requires': ['validated-launch-and-storage', 'authenticated-public-q8-tensor',
                                     'explicit-numerical-contract', 'verified-masked-executor'],
                    }
        return plans
    except (ValueError, KeyError, TypeError, IndexError, OSError, subprocess.SubprocessError):
        return {}
