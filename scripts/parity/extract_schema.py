"""Extract the upstream public GraphQL contract without importing its runtime.

Usage: python3 scripts/parity/extract_schema.py /path/to/ownfoil
The generated contract is checked in; building Rust never requires Python.
"""
import ast
import json
import sys
from pathlib import Path

root = Path(sys.argv[1]) / 'app/gql'
aliases = {}
result = []

def camel(name):
    return name.rstrip('_').split('_')[0] + ''.join(x.title() for x in name.rstrip('_').split('_')[1:])

def typ(node):
    if isinstance(node, ast.Constant):
        return str(node.value) + '!'
    if isinstance(node, ast.Subscript):
        head = ast.unparse(node.value)
        if head in ('Annotated', 'typing_extensions.Annotated'):
            return typ(node.slice.elts[0])
        if head == 'Optional':
            return typ(node.slice).removesuffix('!')
        if head == 'List':
            return '[' + typ(node.slice) + ']!'
        if head == 'Private':
            return None
    name = ast.unparse(node)
    if name in aliases:
        return typ(aliases[name])
    return {'str':'String','int':'Int','float':'Float','bool':'Boolean','strawberry.ID':'ID'}.get(name,name) + '!'

def args(method):
    out=[]
    for arg in method.args.args:
        if arg.arg in ('self','info'):continue
        out.append({'name':camel(arg.arg),'type':typ(arg.annotation)})
    # Optional Python arguments become optional GraphQL arguments even when their values are non-null.
    for arg,default in zip(out[-len(method.args.defaults):],method.args.defaults):
        if not method.args.defaults:break
        try:arg['default']=ast.literal_eval(default)
        except (ValueError,TypeError):pass
    return out

for filename in ('filters.py','types.py','mutations.py','schema.py'):
    tree=ast.parse((root/filename).read_text())
    for node in tree.body:
        if isinstance(node,ast.Assign) and isinstance(node.targets[0],ast.Name):
            aliases[node.targets[0].id]=node.value
        if not isinstance(node,ast.ClassDef):continue
        kind='enum' if any(ast.unparse(b)=='Enum' for b in node.bases) else 'input' if any('strawberry.input' in ast.unparse(d) for d in node.decorator_list) else 'object'
        if kind=='object' and not any('strawberry.type' in ast.unparse(d) for d in node.decorator_list):continue
        item={'name':node.name,'kind':kind,'description':ast.get_docstring(node) or '', 'fields':[]}
        for field in node.body:
            if kind=='enum' and isinstance(field,ast.Assign):
                item['fields'].append({'name':field.targets[0].id});continue
            if isinstance(field,ast.AnnAssign):
                t=typ(field.annotation)
                if t:
                    definition={'name':camel(field.target.id),'type':t,'args':[]}
                    if isinstance(field.value,ast.Call):
                        for kw in field.value.keywords:
                            if kw.arg=='default':
                                try:definition['default']=ast.literal_eval(kw.value)
                                except (ValueError,TypeError):
                                    if isinstance(kw.value,ast.Attribute):definition['default']=kw.value.attr
                    item['fields'].append(definition)
            if isinstance(field,ast.FunctionDef) and any('described_field' in ast.unparse(d) or 'described_mutation' in ast.unparse(d) for d in field.decorator_list):
                item['fields'].append({'name':camel(field.name),'type':typ(field.returns),'args':args(field),'description':ast.get_docstring(field) or ''})
        result.append(item)
Path('ownfoil-rs/src/http/graphql_contract.json').write_text(json.dumps(result,indent=2)+'\n')
