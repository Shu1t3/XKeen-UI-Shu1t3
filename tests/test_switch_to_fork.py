# Regression scenarios with isolated router paths and minimal BusyBox od options.
import os, pathlib, subprocess, tempfile
source=(pathlib.Path(__file__).resolve().parents[1] / 'scripts/switch-to-fork.sh').read_text()
for scenario in ['success', 'bad-elf', 'bad-version', 'failed-start', 'failed-stop', 'download-failure', 'rollback']:
    with tempfile.TemporaryDirectory() as d:
        root=pathlib.Path(d); opt=root/'opt'; commands=root/'commands'; commands.mkdir()
        for p in ['sbin','etc/init.d','etc/xkeen','tmp']:(opt/p).mkdir(parents=True,exist_ok=True)
        old=b'\x7fELFold-binary'; new=b'\x7fELFnew-binary'
        binary=opt/'sbin/xkeen-ui'; binary.write_bytes(old)
        config=opt/'etc/xkeen/xkeen-ui.json'; config.write_text('private-settings')
        running=root/'running'; running.touch()
        attempts=root/'attempts'
        init=opt/'etc/init.d/S99xkeen-ui'
        init.write_text(f'''#!/bin/sh
case "$1" in
 stop) rm -f '{running}'; {'exit 1' if scenario=='failed-stop' else ':'};;
 start)
 {'if [ ! -f "'+str(attempts)+'" ]; then touch "'+str(attempts)+'"; exit 1; fi' if scenario=='failed-start' else ':'}
 touch '{running}';;
esac
'''); init.chmod(0o755)
        mocks={'id':'echo 0', 'opkg':'echo "arch aarch64-3.10_kn 200"', 'pidof':f'test -f "{running}"', 'check-version': 'exit 1' if scenario=='bad-version' else 'echo "XKeen UI v0.0.1-fork.1"', 'sync':':','sleep':':','curl':'exit 22', 'od':'[ "$1" = "-b" ] || exit 1\nexec /usr/bin/od "$@"'}
        for name,body in mocks.items():
            p=commands/name;p.write_text('#!/bin/sh\n'+body+'\n');p.chmod(0o755)
        script=root/'switch.sh';script.write_text(source.replace('/opt/',str(opt)+'/').replace('check_version "$WORK/new"', '"'+str(commands/'check-version')+'" "$WORK/new"'))
        payload=opt/'tmp/new';payload.write_bytes(b'html' if scenario=='bad-elf' else new)
        if scenario=='rollback':
            backup=root/'backup';backup.mkdir();(backup/'xkeen-ui').write_bytes(new);args=['--rollback',str(backup)]
        elif scenario=='download-failure':args=[]
        else:args=['--file',str(payload)]
        result=subprocess.run(['sh',str(script),*args],env={**os.environ,'PATH':str(commands)+':'+os.environ['PATH']},capture_output=True,text=True)
        success=scenario in ['success','rollback']
        assert (result.returncode==0)==success,(scenario,result.stdout,result.stderr)
        assert binary.read_bytes()==(new if success else old),scenario
        assert config.read_text()=='private-settings',scenario
        assert running.exists(),scenario
        assert init.read_text().startswith('#!/bin/sh'),scenario
        if success:
            backups=list((opt/'var/backups/xkeen-ui-switch').iterdir());assert len(backups)==1
            assert (backups[0]/'xkeen-ui').read_bytes()==old
            assert (backups[0]/'xkeen-ui.json').read_text()=='private-settings'
        print('PASS',scenario)

# Exercise the real watchdog with success, failure, and a hung executable.
helper = source[source.index('check_version() ('):source.index('MODE=release')]
for name, body, expected in [('version-success', 'exit 0', True), ('version-failure', 'exit 7', False), ('version-hung', 'while :; do :; done', False)]:
    with tempfile.TemporaryDirectory() as d:
        root = pathlib.Path(d)
        executable = root/'candidate'
        executable.write_text('#!/bin/sh\n'+body+'\n')
        executable.chmod(0o755)
        result = subprocess.run(['sh', '-c', helper.replace('sleep 15 &', 'sleep 1 &')+'\ncheck_version "$1"', 'check', str(executable)], capture_output=True, text=True, timeout=5)
        assert (result.returncode == 0) == expected, (name, result)
        print('PASS', name)
