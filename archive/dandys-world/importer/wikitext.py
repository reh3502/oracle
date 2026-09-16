"""Restricted, budgeted wikitext rendering. No Lua, network, or Python eval."""
import ast
import html
import math
import re
import mwparserfromhell as mw
from mwparserfromhell import nodes


class Unsupported(ValueError):
    pass


def normalize_name(name):
    return str(name).strip().replace('_', ' ')


def expression(text):
    if len(text) > 512:
        raise Unsupported('expression length')
    # MediaWiki round has lower precedence than arithmetic in reviewed templates.
    rounding = re.fullmatch(r'(.+?)\s+round\s+(-?\d+)', text.strip())
    if rounding:
        places = int(rounding[2])
        if abs(places) > 8:
            raise Unsupported('rounding limit')
        # MediaWiki half-away-from-zero, not Python bankers rounding.
        scale = 10 ** places
        number = expression(rounding[1]) * scale
        return math.copysign(math.floor(abs(number) + .5), number) / scale
    text = re.sub(r'\bmod\b', '%', text).replace('^', '**').replace('<>', '!=')
    text = re.sub(r'(?<![<>=!])=(?!=)', '==', text)
    tree = ast.parse(text.strip(), mode='eval')
    if len(list(ast.walk(tree))) > 100:
        raise Unsupported('expression complexity')
    def walk(node):
        if isinstance(node, ast.Constant) and type(node.value) in (int, float):
            result = node.value
        elif isinstance(node, ast.UnaryOp) and isinstance(node.op, (ast.UAdd, ast.USub, ast.Not)):
            value = walk(node.operand)
            result = -value if isinstance(node.op, ast.USub) else (int(not value) if isinstance(node.op, ast.Not) else value)
        elif isinstance(node, ast.BinOp):
            left, right = walk(node.left), walk(node.right)
            if isinstance(node.op, ast.Add): result = left + right
            elif isinstance(node.op, ast.Sub): result = left - right
            elif isinstance(node.op, ast.Mult): result = left * right
            elif isinstance(node.op, ast.Div): result = left / right
            elif isinstance(node.op, ast.Mod): result = left % right
            elif isinstance(node.op, ast.Pow) and abs(right) <= 8: result = left ** right
            else: raise Unsupported('expression operator')
        elif isinstance(node, ast.Compare) and len(node.ops) == 1:
            a,b=walk(node.left),walk(node.comparators[0]);op=node.ops[0]
            if isinstance(op, ast.Eq): result=int(a==b)
            elif isinstance(op, ast.NotEq): result=int(a!=b)
            elif isinstance(op, ast.Lt): result=int(a<b)
            elif isinstance(op, ast.LtE): result=int(a<=b)
            elif isinstance(op, ast.Gt): result=int(a>b)
            elif isinstance(op, ast.GtE): result=int(a>=b)
            else: raise Unsupported('comparison')
        elif isinstance(node, ast.BoolOp):
            values=[bool(walk(v)) for v in node.values]
            result=int(all(values) if isinstance(node.op,ast.And) else any(values))
        else:
            raise Unsupported('expression syntax')
        if not isinstance(result,(int,float)) or not math.isfinite(result) or abs(result)>1e12:
            raise Unsupported('expression magnitude')
        return result
    try:
        return walk(tree.body)
    except (ZeroDivisionError, OverflowError) as exc:
        raise Unsupported('invalid arithmetic') from exc


STRUCTURAL = {'DISPLAYTITLE','Gallery Navigation','TOCright','TOCleft','References','Reflist','LinkToCategory',
              'Thumbnail','Long','Stub','MissingInformation','Limited','Unreleased','LoreImportance','Spoiler',
              'ToonNav','TwistedNav','TrinketNav','FloorNav','MechanicNav','ToonGallNav','TwistedGallNav',
              'GalleryTab','Documentation','TResearch','MastComp','MastStrat','TOTD','TOTWQuests'}


class Renderer:
    def __init__(self, corpus, page):
        self.corpus, self.page = corpus, page
        self.dependencies = set()
        self.unresolved = set()
        self.steps = 0

    def text(self, raw):
        try:
            text = self.render(mw.parse(raw), {}, 0)
        except (Unsupported, RecursionError, SyntaxError, ValueError) as exc:
            self.unresolved.add(str(exc)[:100])
            text = '[Unresolved wiki markup]'
        if '{{' in text or '}}' in text:
            self.unresolved.add('unparsed template markup')
        return re.sub(r'[ \t]+', ' ', re.sub(r'\n\s*\n+', '\n\n', text)).strip()

    def render(self, code, args, depth):
        self.steps += 1
        if depth > 28 or self.steps > 12000:
            raise Unsupported('template expansion budget')
        output=[]
        for node in code.nodes:
            if isinstance(node, nodes.Text): output.append(str(node))
            elif isinstance(node, nodes.HTMLEntity): output.append(html.unescape(str(node)))
            elif isinstance(node, nodes.Comment): continue
            elif isinstance(node, nodes.Argument):
                key=self.render(node.name,args,depth+1).strip()
                if key in args: output.append(self.render(mw.parse(args[key]),{},depth+1))
                elif node.default is not None: output.append(self.render(node.default,args,depth+1))
                # MediaWiki leaves an undefined argument literal. A switch may
                # legitimately fall through to its default on this value.
                # If it reaches the answer, text() marks it unresolved.
                else: output.append(str(node))
            elif isinstance(node,nodes.Wikilink):
                title=self.render(node.title,args,depth+1)
                if not title.lower().startswith(('file:','image:','category:')):
                    output.append(self.render(node.text if node.text is not None else node.title,args,depth+1))
            elif isinstance(node,nodes.ExternalLink):
                if node.title is not None: output.append(self.render(node.title,args,depth+1))
            elif isinstance(node,nodes.Heading): output.append('\n'+self.render(node.title,args,depth+1)+'\n')
            elif isinstance(node,nodes.Tag):
                tag=str(node.tag).lower()
                if tag in {'ref','references','gallery','noinclude'}: continue
                if tag in {'script','style','iframe','object'}: raise Unsupported('unsafe source tag '+tag)
                if tag in {'br','hr'}:output.append('\n')
                elif tag in {'p','div','li','tr','table','tabber','table-progress-tracking'}:
                    output.append('\n'+self.render(node.contents,args,depth+1)+'\n')
                elif tag in {'td','th'}:output.append(self.render(node.contents,args,depth+1)+' | ')
                elif tag in {'b','i','u','s','strong','em','span','small','big','center','font','includeonly','onlyinclude','nowiki','sup','sub','h4','h3','h2'}:
                    output.append(self.render(node.contents,args,depth+1))
                else: raise Unsupported('unrecognized tag '+tag)
            elif isinstance(node,nodes.Template):
                try: output.append(self.template(node,args,depth+1))
                except (Unsupported, SyntaxError, ValueError, RecursionError) as exc:
                    # Do not use an unknown nested expansion to decide a conditional.
                    if depth: raise Unsupported(str(exc)) from exc
                    self.unresolved.add(normalize_name(node.name))
                    output.append('[Unresolved template: '+normalize_name(node.name)+']')
            else: raise Unsupported('unknown wiki node')
        result=''.join(output)
        if len(result)>256000:raise Unsupported('expanded text limit')
        return result

    def template(self,node,outer,depth):
        name=normalize_name(node.name)
        def evaluate(raw):return self.render(mw.parse(str(raw)),outer,depth+1)
        def param(index,default=''):
            return evaluate(node.get(index).value) if node.has(index) else default
        if name.startswith('DISPLAYTITLE:') or name in STRUCTURAL or name.endswith('GallNav'):return ''
        if name in {'Toons','Twisted','Trinket','Item','Infobox Event','Infobox_event'}:return ''
        if name=='PAGENAME':return self.page['title']
        if name in {'!','='}:return '|' if name=='!' else '='
        if name in {'CI','TI','II','CDI','Type','ToonBox','TwistedBox','TrinketBox'}:
            return ' '+param(1)+' '
        if name in {'Small','Center','Text'}:return param(1)
        if name=='Color':return param(1)
        if name=='Tooltip':return param(1)+' ('+param(2)+')'
        if name=='Heart':return 'Heart'
        if name=='Main Heart':return ''
        if name=='Star':return '★'
        if name=='StarRow':return '★'*int(param(1)) if param(1).isdigit() and 0<=int(param(1))<=6 else '[unknown stars]'
        if name=='Tape':return 'Tapes'
        if name=='Robux':return 'Robux'
        if name=='Currency':return param(2)+' '+param(1)
        if name=='Debuff':return param(1)+' '+param(2)
        if name in {'Mastery','MastStrat'}:
            return '\n'.join('- '+evaluate(p.value) for p in node.params if str(p.name).strip().isdigit())
        if name.startswith('#'):
            function,_,head=name.partition(':');head=evaluate(head).strip()
            if function=='#if':return param(1) if head else param(2)
            if function=='#ifeq':return param(2) if head==param(1).strip() else param(3)
            if function=='#ifexpr':return param(1) if expression(head) else param(2)
            if function=='#expr':
                value=expression(head)
                return str(int(value)) if value==int(value) else format(value,'.12g')
            if function=='#switch':
                matched=False;default=''
                for index,p in enumerate(node.params):
                    key='#default' if str(p.name).strip()=='#default' else evaluate(p.name).strip()
                    if not p.showkey:
                        if index==len(node.params)-1:default=p.value
                        elif evaluate(p.value).strip()==head:matched=True
                    elif key=='#default':default=p.value
                    elif matched or key==head:return evaluate(p.value)
                return evaluate(default)
            if function=='#tag' and head in {'tabber','span','div'}:return param(1)
            raise Unsupported('unsupported parser function '+function)
        title='Template:'+name
        source=self.corpus.by_title.get(title)
        if source is None:raise Unsupported('missing template '+name)
        self.dependencies.add(source['source']['id'])
        raw=source['raw']
        redirect=re.match(r'\s*#redirect\s*\[\[([^]#]+)',raw,re.I)
        if redirect:
            target=redirect[1]
            if not target.startswith('Template:'):raise Unsupported('template redirect namespace')
            replacement=mw.parse(str(node)).filter_templates(recursive=False)[0]
            replacement.name=target[len('Template:'):]
            return self.template(replacement,outer,depth+1)
        only=re.findall(r'<onlyinclude>(.*?)</onlyinclude>',raw,re.S|re.I)
        raw=''.join(only) if only else re.sub(r'<noinclude>.*?(?:</noinclude>|$)','',raw,flags=re.S|re.I)
        raw=re.sub(r'</?includeonly>','',raw,flags=re.I)
        arguments={}
        for p in node.params:
            key=evaluate(p.name).strip()
            if key in arguments:raise Unsupported('duplicate template argument')
            arguments[key]=evaluate(p.value)
        return self.render(mw.parse(raw),arguments,depth+1)

    def citations(self,section,raw):
        citations=[self.corpus.citation(self.page,section,raw)]
        for identity in sorted(self.dependencies):
            source=next(p for p in self.corpus.rows if p['source']['id']==identity)
            citations.append(self.corpus.citation(source,'Template definition',source['raw']))
        return citations
