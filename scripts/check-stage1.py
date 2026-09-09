#!/usr/bin/env python3
"""Stage 1 acceptance drill: real CLI, disposable PostgreSQL, isolated restore.

Requires PostgreSQL 18 server/client tools. No Discord/provider traffic. The
optional --postgres-bin supports an extracted local PostgreSQL distribution.
"""
import argparse, datetime, hashlib, json, os, pathlib, shutil, signal, subprocess, tempfile, time
ROOT=pathlib.Path(__file__).resolve().parent.parent

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--postgres-bin',type=pathlib.Path)
    parser.add_argument('--postgres-share',type=pathlib.Path)
    parser.add_argument('--postgres-lib',type=pathlib.Path)
    args=parser.parse_args()
    env=os.environ.copy()
    for key in ['DISCORD_TOKEN','GEMINI_KEY','GEMINI_API_KEY']:
        env.pop(key,None)
    if args.postgres_lib:
        env['LD_LIBRARY_PATH']=str(args.postgres_lib.resolve())+(':'+env['LD_LIBRARY_PATH'] if env.get('LD_LIBRARY_PATH') else '')
    pg_bin=args.postgres_bin.resolve() if args.postgres_bin else pathlib.Path(shutil.which('pg_ctl') or '/usr/lib/postgresql/18/bin/pg_ctl').parent
    for name in ['initdb','pg_ctl','createdb','pg_dump','pg_restore']:
        if not (pg_bin/name).is_file():
            raise SystemExit(f'Missing PostgreSQL tool: {pg_bin/name}; supply --postgres-bin')
    (ROOT/'target').mkdir(exist_ok=True)
    run=pathlib.Path(tempfile.mkdtemp(prefix='stage1-',dir=ROOT/'target'))
    sock=run/'socket';sock.mkdir(mode=0o700)
    log=run/'postgres.log'
    def command(argv,**kwargs):
        return subprocess.run([str(a) for a in argv],env=env,cwd=ROOT,check=True,timeout=600,**kwargs)
    init=[pg_bin/'initdb','-D',run/'pg','--locale=C','--encoding=UTF8','--auth=trust']
    if args.postgres_share:init+=['-L',args.postgres_share.resolve()]
    with (run/'initdb.log').open('w') as out:command(init,stdout=out,stderr=subprocess.STDOUT)
    command([pg_bin/'pg_ctl','-D',run/'pg','-l',log,'-o',f"-k {sock} -h '' -p 55439",'-w','start'],stdout=subprocess.DEVNULL)
    children=[]
    try:
        for name in ['contract','restore','cli','cli_restore']:
            command([pg_bin/'createdb','-h',sock,'-p','55439',name],stdout=subprocess.DEVNULL)
        user=command(['id','-un'],capture_output=True,text=True).stdout.strip()
        def url(db):return f'postgresql://{user}@localhost/{db}?host={sock}&port=55439'
        env['ORACLE_TEST_POSTGRES_URL']=url('contract')
        env['ORACLE_TEST_POSTGRES_RESTORE_URL']=url('restore')
        env['ORACLE_TEST_PG_BIN']=str(pg_bin)
        command(['python3','scripts/prepare-serenity.py'])
        command(['cargo','test','--locked','--workspace'])
        command(['cargo','build','--locked','-p','oracle'])
        binary=ROOT/'target/debug/oracle'
        source=run/'source';source.mkdir();config=source/'oracle.json'
        env['ORACLE_DATABASE_URL']=url('cli')
        def cli(path,*args):
            p=command([binary,'--config',path,'--pg-dump',pg_bin/'pg_dump','--pg-restore',pg_bin/'pg_restore',*args],capture_output=True,text=True)
            return json.loads(p.stdout)
        initial=cli(config,'init','--postgres-url-env','ORACLE_DATABASE_URL')
        assert initial['modules_loaded']==0 and initial['ai_available'] is False
        data=json.loads(config.read_text());data['guilds']=[{'guild':'123456789012345678','operators':[]}];config.write_text(json.dumps(data))
        out=(run/'host.out').open('w');err=(run/'host.err').open('w')
        host=subprocess.Popen([str(binary),'--config',str(config),'--pg-dump',str(pg_bin/'pg_dump'),'--pg-restore',str(pg_bin/'pg_restore'),'serve'],env=env,cwd=ROOT,stdout=out,stderr=err)
        children.append(host)
        deadline=time.monotonic()+30
        while time.monotonic()<deadline:
            assert host.poll() is None,'PostgreSQL host exited before READY (see retained logs)'
            try:
                ready=json.JSONDecoder().raw_decode((run/'host.out').read_text().lstrip())[0]
                if ready.get('event')=='ready':break
            except json.JSONDecodeError:
                pass
            time.sleep(.05)
        else:raise AssertionError('PostgreSQL host readiness timeout')
        assert ready['event']=='ready' and ready['status']['modules_loaded']==0
        state=cli(config,'status');assert len(state['guilds'])==1
        pause=cli(config,'control','--guild','123456789012345678','pause');assert pause['guild']['paused']
        resume=cli(config,'control','--guild','123456789012345678','resume');assert not resume['guild']['paused']
        backup=run/'backup';cli(config,'backup','--output',backup)
        target=run/'isolated';target.mkdir();restore_config=target/'oracle.json'
        data['database']={'backend':'postgres','url_env':'ORACLE_RESTORE_DATABASE_URL'}
        restore_config.write_text(json.dumps(data));env['ORACLE_RESTORE_DATABASE_URL']=url('cli_restore')
        restored=cli(restore_config,'restore','--backup',backup)
        assert restored['deployment']!=state['deployment']
        assert restored['guilds'][0]['paused']
        assert cli(config,'status')['guilds'][0]['paused'] is False,'Restore changed original deployment'
        host.send_signal(signal.SIGTERM);assert host.wait(timeout=25)==0
        children.remove(host);out.close();err.close()
        text=(run/'host.out').read_text();decoder=json.JSONDecoder();events=[]
        while text.strip():
            text=text.lstrip();event,end=decoder.raw_decode(text);events.append(event);text=text[end:]
        assert events[-1]['event']=='stopped'
        assert events[-1]['tasks']['stats']['counts']['running']==0
        artifacts=ROOT/'evidence';artifacts.mkdir(exist_ok=True)
        sources=sorted([*ROOT.glob('crates/**/*.rs'),*ROOT.glob('crates/**/Cargo.toml'),ROOT/'Cargo.toml',ROOT/'Cargo.lock',pathlib.Path(__file__).resolve(),ROOT/'scripts/prepare-serenity.py',ROOT/'patches/serenity-preserve-unknown-dispatch.patch'])
        report={'status':'passed','checked_at_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'checks':['workspace unit/integration tests','SQLite CLI boot/control/backup/restore','PostgreSQL repository contract and native restore','PostgreSQL live CLI zero-module boot','PostgreSQL local pause/resume without Gemini','PostgreSQL backup while source running','isolated paused deployment with new identity','original deployment unchanged','SIGTERM joins host tasks'],'postgres_version':command([pg_bin/'postgres','--version'],capture_output=True,text=True).stdout.strip(),'rustc':command(['rustc','--version'],capture_output=True,text=True).stdout.strip(),'source_sha256':{str(p.relative_to(ROOT)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sources}}
        (artifacts/'stage1-local.json').write_text(json.dumps(report,indent=2)+'\n')
        print(json.dumps({'stage1':'passed','report':'evidence/stage1-local.json'}))
    finally:
        for child in children:
            child.kill();child.wait(timeout=10)
        subprocess.run([str(pg_bin/'pg_ctl'),'-D',str(run/'pg'),'-m','fast','-w','stop'],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=30)
        print('Retained isolated drill data:',run)
if __name__=='__main__':main()
