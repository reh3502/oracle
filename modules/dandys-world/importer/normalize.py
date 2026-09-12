"""Normalize an attributed offline wiki snapshot to the Rust catalog schema."""
import argparse
import collections
import json
import re
from pathlib import Path
import mwparserfromhell as mw
from wiki_source import Corpus, ImportError, ORIGIN, sha
from wikitext import Renderer, normalize_name

STAT_HASH='8629aabe5ba771b1ccdc644f44934e24e219054847ef4f1e1e069f71859ff8f4'
STAT_FIELDS={'skill_check':'Skill','movement_speed':'Move','stamina':'Stam','stealth':'Stealth','extraction_speed':'Extract'}
INFOBOXES={'Toons','Twisted','Trinket','Item','Infobox Event','EffectofEvent'}
EXCLUDED_SECTIONS={'gallery','audio','dialogue','dialogues','interactions','appearance','personality','trivia','references','navigation','skins','stickers','sounds','ambience'}
PRIMARY_MECHANICS={'Health','Skill Check','Movement Speed','Stamina','Stealth','Extraction Speed','Statistics','Research','Mastery','Ability','Abilities','Ichor','Tapes','Cards','Panic Mode','Blackouts','Floor Events','Elevator','Lobby','Trinkets','Items','Toons','Twisteds','Floors','Machines'}


def key(text):
    return re.sub(r'[^a-z0-9]+','_',text.lower()).strip('_')


def templates(raw):
    return mw.parse(raw).filter_templates(recursive=False)


def redirect(raw):
    match=re.match(r'\s*#redirect\s*\[\[([^]|]+)',raw,re.I)
    if not match:return None
    title,_,anchor=match[1].partition('#')
    return title.replace('_',' ').strip(),anchor.replace('_',' ').strip()


def table_rows(table):
    """Expand table spans while retaining original cell text and header identity."""
    leading=[];groups=[]
    for node in table.contents.filter_tags(recursive=False):
        if str(node.tag) in {'th','td'}:leading.append(node)
        elif str(node.tag)=='tr':
            if leading:groups.append(leading);leading=[]
            groups.append([t for t in node.contents.filter_tags(recursive=False) if str(t.tag) in {'th','td'}])
    if leading:groups.append(leading)
    carry={};result=[]
    for cells in groups:
        row={col:cell for col,(left,cell) in carry.items()}
        carry={col:(left-1,cell) for col,(left,cell) in carry.items() if left>1}
        col=0
        for tag in cells:
            while col in row:col+=1
            def span(name):
                if tag.has(name):
                    value=int(str(tag.get(name).value).strip())
                else:
                    # MediaWiki source sometimes omits whitespace after a quoted style.
                    # Read only literal span attributes, excluding commented-out attributes.
                    prefix=str(tag).split('|',2)[1] if '|' in str(tag) else ''
                    prefix=re.sub(r'<!--.*?-->','',prefix,flags=re.S)
                    match=re.search(r'(?<![a-zA-Z_])'+name+r'''\s*=\s*["']?(\d+)''',prefix)
                    value=int(match[1]) if match else 1
                if not 1<=value<=100:raise ImportError('table span limit')
                return value
            width,height=span('colspan'),span('rowspan')
            cell={'raw':str(tag.contents),'header':str(tag.tag)=='th'}
            for offset in range(width):
                if col+offset in row:raise ImportError('overlapping table spans')
                row[col+offset]=cell
                if height>1:carry[col+offset]=(height-1,cell)
            col+=width
        if row:result.append([row.get(i) for i in range(max(row)+1)])
    return result


class Normalizer:
    def __init__(self,corpus):
        self.corpus=corpus;self.entities={};self.by_name={};self.excluded=[];self.unresolved=[]
        self.redirects={r['title']:redirect(r['raw']) for r in corpus.rows if r['namespace']==0 and redirect(r['raw'])}
        self.quality=collections.Counter();self.fact_counter=collections.Counter()

    def entity(self,row,kind,name=None,identity=None):
        name=name or row['title'];identity=identity or 'page:'+str(row['page_id'])
        if identity in self.entities:return self.entities[identity]
        names={normalize_name(t.name).lower() for t in templates(row['raw'])}
        warnings=[]
        for flag in ['missinginformation','stub','limited','unreleased']:
            if flag in names:warnings.append('Wiki marks this page '+flag);self.quality[flag]+=1
        availability='historical' if 'unreleased' in names else 'supported'
        if kind=='event':warnings.append('Event information does not verify that an event is active today.')
        entity={'id':identity,'kind':kind,'name':name,'aliases':[],'availability':availability,'warnings':warnings,'facts':[],'relationships':[]}
        self.entities[identity]=entity;self.by_name.setdefault(name,[]).append(entity)
        return entity

    def fact(self,entity,row,field,raw,section,state='supported',value=None,unit=None,conditions=None):
        if not raw.strip():
            raw='' # Blank value is evidence of an explicitly unknown field.
        renderer=Renderer(self.corpus,row)
        text=renderer.text(raw)
        if any(marker in text.lower() for marker in ['this table is outdated','may include inaccuracies','unknown if the buff was intended']):
            state='unverified';value=None
        if not text or text.strip().lower() in {'tba','tbd','wip','unknown','n/a','none available'}:
            state='unknown';text='The wiki does not provide a verified value.';value=None
        if renderer.unresolved:
            state='unverified';text='Unresolved wiki content: '+', '.join(sorted(renderer.unresolved));value=None
        if entity['availability']=='historical':state='historical'
        if len(text)>8000:
            state='unverified';text='This source passage exceeds the answerable field limit; consult the cited section.';value=None
        self.fact_counter[entity['id']]+=1
        raw_evidence=raw if raw else row['raw'][:200]
        fact={'id':entity['id']+':fact:'+str(self.fact_counter[entity['id']]),'key':field,'text':text,
              'value':value if value is not None else (text if state in {'supported','historical'} else None),
              'unit':unit,'conditions':conditions or [],'state':state,'citations':renderer.citations(section,raw_evidence)}
        entity['facts'].append(fact)
        return fact

    def stat(self,entity,row,field,raw):
        explicit_stealth=re.fullmatch(r'\s*\{\{Star\}\}(?:\s|<br\s*/?>)*\{\{Small\|\((-?\d+)\)\}\}\s*',raw)
        if field=='stealth' and explicit_stealth:
            return self.fact(entity,row,field,raw,'Infobox / '+field,
                             value={'stars':1,'priority':int(explicit_stealth[1])},
                             conditions=['explicit source statistic; not calculated from star rating'])
        source=self.corpus.by_title.get('Template:StatComp')
        instances=[t for t in mw.parse(raw).filter_templates() if normalize_name(t.name)=='StatComp']
        valid=source and sha(source['raw'])==STAT_HASH
        values=[]
        for t in instances:
            kind=str(t.get(1).value).strip() if t.has(1) else ''
            stars=str(t.get(2).value).strip() if t.has(2) else ''
            if kind!=STAT_FIELDS[field] or stars not in {'1','2','3','4','5'}:valid=False;continue
            n=int(stars);data={'stars':n}
            if kind=='Move':data.update(walk=n*2.5+7.5,sprint=n*2.5+17.5)
            elif kind=='Stam':data['capacity']=(n+3)*25
            elif kind=='Stealth':data['priority']=(n-1)*5
            elif kind=='Extract':data['rate']=(n**4-10*n**3+47*n*n-38*n+360)/480
            elif kind=='Skill':data.update(chance_percent=25,size=n*50,value=(n+1)*.5,speed=n)
            values.append(data)
        simple=len(instances)==1 and str(instances[0]).strip()==raw.strip()
        conditions=['base statistics'] if simple else [
            'Do not compare as unconditional base statistics. Source conditions: '+Renderer(self.corpus,row).text(raw)]
        value=values[0] if valid and simple else ({'variants':values} if valid and values else None)
        result=self.fact(entity,row,field,raw,'Infobox / '+field,value=value,conditions=conditions)
        if not valid or not values:
            result['state']='unverified';result['value']=None
            result['text']='The statistic markup is not covered by the reviewed StatComp adapter; consult the source.'
        return result

    def infobox(self,entity,row,box):
        values={};duplicate=set()
        for p in box.params:
            name=str(p.name).strip()
            if name in values:duplicate.add(name)
            values[name]=str(p.value).strip()
        required=[]
        for name,raw in values.items():
            if name in STAT_FIELDS:
                f=self.stat(entity,row,name,raw)
            elif name in {'title1','title','name','image1','caption-image1','caption-image','skins'} or name.startswith('heart'):
                continue
            elif name.startswith('requirement_'):
                r=Renderer(self.corpus,row);display=r.text(raw)
                choices=re.split(r'<br\s*/?>\s*or\s*<br\s*/?>',raw,flags=re.I)
                if len(choices)>1:
                    requirement={'any':[{'text':Renderer(self.corpus,row).text(c)} for c in choices]}
                else:requirement={'text':display}
                required.append((name,raw,requirement,r.unresolved));continue
            elif name in {'ability_1','ability_2','effect','upon_use','chance','type','speed','mechanic','attention_span','detection_range','gender','pronouns','designation','release_date','start_date','end_date','date','currency','duration'}:
                field='effect' if name=='upon_use' else name
                f=self.fact(entity,row,field,raw,'Infobox / '+name)
            else:continue
            if name in duplicate:f['state']='conflicting';f['value']=None;f['text']='The source repeats this field with different entries.'
        if required:
            required.sort(key=lambda x:x[0])
            # Preserve source logical alternatives inside the declared requirement conjunction.
            fs=[self.fact(entity,row,'requirement_detail',raw,'Infobox / '+name) for name,raw,_,_ in required]
            combined={**fs[0],'id':entity['id']+':requirements','key':'requirements',
                      'text':'\nAND\n'.join(f['text'] for f in fs),
                      'value':{'all':[item[2] for item in required]},
                      'citations':[c for f in fs for c in f['citations']],
                      'conditions':['All listed requirements apply; alternatives remain inside each requirement.']}
            combined['state']='unverified' if any(x[3] for x in required) else ('conflicting' if any(x[0] in duplicate for x in required) else fs[0]['state'])
            if combined['state'] not in {'supported','historical'}:combined['value']=None
            entity['facts']=[f for f in entity['facts'] if f not in fs];entity['facts'].append(combined)
        if entity['kind'] in {'toon','npc'} and row['title']!='Minor Characters':
            hearts=[v for k,v in values.items() if k.startswith('heart') and v]
            if hearts and all(v in {'Heart','Main Heart'} for v in hearts):
                count=hearts.count('Heart')
                if count:
                    raw=next(str(p.value).strip() for p in box.params if str(p.name).strip().startswith('heart') and str(p.value).strip()=='Heart')
                    f=self.fact(entity,row,'health',raw,'Infobox health slots',value=count,unit='hearts',conditions=['maximum starting health as shown by normal Heart slots'])
                    f['text']=f'Maximum starting health: {count} hearts. Main Heart decoration is not counted.'
                    f['citations']=[self.corpus.citation(row,'Infobox health slots',str(box))]
                    health=self.corpus.by_title.get('Health')
                    if health:f['citations'].append(self.corpus.citation(health,'Health explanation',health['raw'].split('==Losing Hearts==')[0]))

    def classify(self,row):
        names=[normalize_name(t.name) for t in templates(row['raw'])]
        cats=[str(link.title)[9:].strip() for link in mw.parse(row['raw']).filter_wikilinks() if str(link.title).startswith('Category:')]
        title=row['title']
        if 'Toons' in names and title!='Minor Characters':return 'npc' if title in {'Dandy','Dyle'} else 'toon'
        if 'Twisted' in names:return 'twisted'
        if 'Trinket' in names:return 'trinket'
        if 'Infobox Event' in names or title in {'Events','Christmas Event','Easter Event','Halloween Event','Collabs'}:return 'event'
        if any(x in cats for x in {'Floors','Main Floors','Holiday Floors','Challenge Floors'}) and title not in {'Floors','Challenge Floors','Holiday Floors'}:return 'floor'
        if 'Item' in names and title not in PRIMARY_MECHANICS:return 'item'
        if title in PRIMARY_MECHANICS or any(x in cats for x in {'Mechanic','Mechanics','Statistics'}):return 'mechanic'
        if any(x in cats for x in {'Humans','Characters','Toon Handlers'}):return 'npc'
        return 'topic'

    def sections(self,entity,row,code=None):
        code=code or mw.parse(row['raw'])
        for section in code.get_sections(include_lead=True,flat=True):
            headings=section.filter_headings(recursive=False)
            heading=Renderer(self.corpus,row).text(str(headings[0].title)).strip() if headings else 'Overview'
            heading_key=key(heading)
            if heading_key in EXCLUDED_SECTIONS or any(word in heading_key for word in ['gallery','audio','dialogue','unused','old_','history','changelog']):continue
            raw=str(section)
            if headings:raw=raw[raw.find(str(headings[0]))+len(str(headings[0])):]
            # Infoboxes are parsed into fields. Navigation/decorative markup is skipped.
            parsed=mw.parse(raw)
            for t in list(parsed.filter_templates(recursive=False)):
                if normalize_name(t.name) in INFOBOXES:parsed.remove(t)
            # Removing nodes changes the fragment; each original paragraph remains citeable.
            chunks=re.split(r'\n\s*\n',str(parsed))
            for chunk in chunks:
                if not chunk.strip() or chunk not in row['raw']:continue
                renderer=Renderer(self.corpus,row);display=renderer.text(chunk)
                if not display.strip():continue
                # Full tables have dedicated adapters below; avoid huge semantically flattened tables.
                if any(str(t.tag)=='table' for t in mw.parse(chunk).filter_tags()):continue
                state='unverified' if any(term in heading_key for term in ['strateg','lore','tips','recommend']) else 'supported'
                self.fact(entity,row,heading_key,chunk,heading,state=state)

    def new_named(self,row,name,kind,section):
        existing=self.by_name.get(name,[])
        entity=next((e for e in existing if e['kind']==kind),None)
        if entity:return entity
        original=self.corpus.by_title.get(name)
        identity='page:'+str(original['page_id']) if original and redirect(original['raw']) else f"page:{row['page_id']}:{kind}:{key(name)}"
        return self.entity(row,kind,name,identity)

    def tables(self):
        for title,kind in [('Items','item'),('Floors','floor')]:
            row=self.corpus.by_title.get(title)
            if row is None:continue
            section=next((s for s in mw.parse(row['raw']).get_sections(flat=True) if s.filter_headings() and str(s.filter_headings()[0].title).strip()=='List of '+title),None)
            if section is None:raise ImportError('Missing authoritative '+title+' table')
            table=next((t for t in section.filter_tags() if str(t.tag)=='table'),None)
            if table is None:raise ImportError('Missing table markup')
            rows=table_rows(table)
            for cells in rows:
                if not cells or not cells[0] or cells[0]['header']:continue
                raw=cells[0]['raw']
                if kind=='item':
                    name=None
                    for t in mw.parse(raw).filter_templates():
                        if normalize_name(t.name) in {'II','TI'} and t.has(1):name=str(t.get(1).value).strip();break
                        if normalize_name(t.name)=='Tape':name='Tapes';break
                    if not name:raise ImportError('Unknown item name cell')
                    if len(cells)!=9:raise ImportError('Item table shape changed')
                    cells=[c if c is not None else {'raw':'','header':False} for c in cells]
                    entity=self.new_named(row,name,kind,'List of Items')
                    for index,field in [(1,'rarity'),(2,'effect'),(3,'category'),(4,'shop_only')]:
                        self.fact(entity,row,field,cells[index]['raw'],'List of Items / '+name+' / '+field)
                    # Keep each discount condition explicit, with both header and row provenance.
                    for offset,condition in enumerate(['normal','Dandy Plush','Frugal Card','both discounts']):
                        cell=cells[5+offset]['raw'];display=Renderer(self.corpus,row).text(cell)
                        price=self.fact(entity,row,'price',cell,'List of Items / '+name+' / '+condition,
                                        value=int(display) if display.isdigit() else display,unit='Tapes',conditions=[condition])
                        price['text']=f'{condition}: {price["text"]} Tapes' if display.isdigit() else display
                        price['citations'].append(self.corpus.citation(row,'List of Items / price headers',str(table.contents).split('|-')[0]))
                    if 'removed' in entity['facts'][2]['text'].lower() or name=='Enigma Candy':
                        entity['availability']='historical';entity['warnings'].append('Removed item per source table category.')
                        for f in entity['facts']:f['state']='historical'
                else:
                    links=[l for l in mw.parse(raw).filter_wikilinks() if not str(l.title).startswith('File:')]
                    if not links:continue
                    name=str(links[0].title).split('#')[0].strip()
                    if len(cells)!=5 or any(c is None for c in cells):raise ImportError('Floor table shape changed')
                    entity=self.new_named(row,name,kind,'List of Floors')
                    self.fact(entity,row,'variants',cells[3]['raw'],'List of Floors / '+name+' / variants')
                    f=self.fact(entity,row,'requirements',cells[4]['raw'],'List of Floors / '+name+' / requirements')
                    if 'unreleased' in f['text'].lower():
                        entity['availability']='historical';entity['warnings'].append('Unreleased/unconfirmed floor in source table.')
                        for fact in entity['facts']:fact['state']='historical'

    def machine_sections(self):
        row=self.corpus.by_title.get('Machines')
        if not row:return
        for section in mw.parse(row['raw']).get_sections(levels=[3],include_lead=False):
            header=section.filter_headings()[0];name=str(header.title).strip()
            if name not in {'Default Machine','Circle Machine','Treadmill Machine','Barnaby Machine','Duo Machines'}:continue
            entity=self.new_named(row,name,'machine','Mechanics / '+name)
            self.sections(entity,row,section)

    def aliases_and_relationships(self):
        for name,(target,anchor) in self.redirects.items():
            # Named item/machine rows are real targets even when wiki redirects to a shared table.
            if name in self.by_name:continue
            seen={name}
            while target in self.redirects:
                if target in seen:target='';break
                seen.add(target);target,new_anchor=self.redirects[target];anchor=anchor or new_anchor
            candidates=self.by_name.get(anchor,[]) or self.by_name.get(target,[])
            if candidates:
                for e in candidates:
                    if name not in e['aliases']:e['aliases'].append(name)
            else:
                row=self.corpus.by_title[name]
                self.unresolved.append({'page_id':row['page_id'],'title':name,'reason':'Redirect target excluded, missing, or cyclic: '+target})
        for entity in list(self.entities.values()):
            if entity['kind']=='twisted' and entity['name'].startswith('Twisted '):
                short=entity['name'][8:];other=next((x for x in self.by_name.get(short,[]) if x['kind'] in {'toon','npc'}),None)
                if other:
                    # The paired entity name is explicit on these corresponding canonical pages.
                    row=self.corpus.by_title[entity['name']]
                    if short in row['raw']:
                        entity['aliases'].append(short)
                        cites=[self.corpus.citation(row,'Counterpart identity',row['raw'][:300])]
                        entity['relationships'].append({'relation':'toon_counterpart','target_id':other['id'],'citations':cites})
                        other['relationships'].append({'relation':'twisted_counterpart','target_id':entity['id'],'citations':cites})
            for fact in entity['facts']:
                if fact['key']=='requirements':
                    for c in fact['citations']:
                        for link in mw.parse(c['quote']).filter_wikilinks():
                            for target in self.by_name.get(str(link.title),[]):
                                if target['id']!=entity['id']:
                                    relation={'relation':'requirement_reference','target_id':target['id'],'citations':[c]}
                                    if relation not in entity['relationships']:entity['relationships'].append(relation)
            entity['aliases']=sorted(set(entity['aliases'])-{entity['name']})

    def build(self):
        for row in self.corpus.rows:
            if row['namespace']!=0 or redirect(row['raw']):continue
            title=row['title']
            if '/' in title or title.startswith('Unused Content') or any(word in title for word in ['Dialogue','Skins','Stickers','Changelog','Main Page','Disambiguation']):
                self.excluded.append({'page_id':row['page_id'],'title':title,'reason':'Historical, media/dialogue, navigation, or subpage outside current game lookup scope.'});continue
            entity=self.entity(row,self.classify(row))
            for box in templates(row['raw']):
                if normalize_name(box.name) in INFOBOXES:
                    if title=='Minor Characters':
                        name=next((str(p.value).strip() for p in box.params if str(p.name).strip() in {'title1','title'}),'')
                        if name:
                            child=self.new_named(row,name,'npc','Minor character profile');self.infobox(child,row,box)
                    else:self.infobox(entity,row,box)
            self.sections(entity,row)
        self.tables();self.machine_sections();self.aliases_and_relationships()
        self.apply_reviews()
        counts=collections.Counter(e['kind'] for e in self.entities.values())
        required={'toon','twisted','npc','floor','machine','mechanic','trinket','item','event'}
        if not required.issubset(counts):raise ImportError('Required category absent: '+str(required-counts.keys()))
        # Every canonical article has either a page entity or an explicit exclusion.
        main=[r for r in self.corpus.rows if r['namespace']==0]
        excluded={e['page_id'] for e in self.excluded}
        for row in main:
            if not redirect(row['raw']) and 'page:'+str(row['page_id']) not in self.entities and row['page_id'] not in excluded:
                raise ImportError('Unaccounted canonical article '+row['title'])
        coverage={'discovered_pages':len(self.corpus.rows),'imported_pages':len(self.corpus.rows),
                  'namespace_counts':self.corpus.namespace_counts,'nonredirect_articles':sum(not redirect(r['raw']) for r in main),
                  'redirects':len(self.redirects),'entities_by_kind':dict(counts),'excluded':self.excluded,
                  'unresolved_redirects':self.unresolved,
                  'warnings':['Coverage is relative to the saved discovery indexes, not the live wiki.',
                              'Unresolved templates and known source conflicts remain explicit; historical facts are labelled.']}
        return {'schema_version':1,'adapter_version':'dw-wiki/1.0','source_origin':ORIGIN,
                'crawl_started_at':self.corpus.manifest['started_at'],'crawl_completed_at':self.corpus.manifest['completed_at'],
                'sources':[r['source'] for r in self.corpus.rows],'entities':sorted(self.entities.values(),key=lambda x:x['id']), 'coverage':coverage}

    def apply_reviews(self):
        path=Path(__file__).with_name('source_reviews.json')
        for rule in json.loads(path.read_text()):
            rows=[self.corpus.by_title.get(title) for title in rule['revisions']]
            reviewed=all(row and row['source']['revision_id']==rule['revisions'][row['title']] for row in rows)
            for entity in self.entities.values():
                if entity['name']!=rule['entity']:continue
                for fact in entity['facts']:
                    if fact['key'] not in rule['keys']:continue
                    if not reviewed:
                        fact['state']='unverified';fact['value']=None
                        reason='Previously disputed field requires review against the new source revisions.'
                        if reason not in entity['warnings']:entity['warnings'].append(reason)
                        continue
                    if rule.get('contains') and not any(t in fact['text'] for t in rule['contains']):continue
                    fact['state']='conflicting';fact['value']=None
                    fact['conditions'].append(rule['reason'])
                    for title,fragment in rule['evidence'].items():
                        row=self.corpus.by_title[title]
                        fact['citations'].append(self.corpus.citation(row,'Reviewed source discrepancy',fragment))
                    if rule['reason'] not in entity['warnings']:entity['warnings'].append(rule['reason'])


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--corpus',type=Path,required=True);parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args()
    result=Normalizer(Corpus.open(args.corpus)).build()
    encoded=json.dumps(result,ensure_ascii=False,sort_keys=True,separators=(',',':')).encode()
    if len(encoded)>128*1024*1024:raise ImportError('Normalized snapshot size limit')
    args.output.parent.mkdir(parents=True,exist_ok=True)
    # Candidate only. The Rust store validates and atomically publishes it.
    with args.output.open('xb') as handle:handle.write(encoded)
    print(json.dumps({'candidate':str(args.output),'bytes':len(encoded),'coverage':result['coverage'],'fact_states':dict(collections.Counter(f['state'] for e in result['entities'] for f in e['facts']))},indent=2))

if __name__=='__main__':main()
