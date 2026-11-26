// Plays the line on the landing page: one made-up project moving through AgentFlow, step by step,
// with the other agents' work going by in the log. Nothing runs with reduced motion, and it pauses
// while it's off screen or the tab is hidden.

type Cat = 'agents' | 'api' | 'builds' | 'conversations' | 'you' | 'system';

// The app's state tones (src/web/tone.rs).
const TONE: Record<string, string> = {
  doing: 'accent', active: 'accent',
  done: 'ok', merged: 'ok', ready: 'ok', closed: 'ok', answered: 'ok',
  blocked: 'bad', draft: 'info', todo: '',
};
const LABEL: Record<Cat, string> = {
  agents: 'Agents', api: 'API', builds: 'Builds', conversations: 'Conversations', you: 'You', system: 'System',
};

// What the other agents do meanwhile.
const CHATTER: [Cat, string, string, string][] = [
  ['agents', 'orbit', 'orbit', 'update_task T-12 → done'],
  ['builds', 'orbit', 'orbit', 'Job #339 cargo build --release on forge: running'],
  ['agents', 'kiln', 'kiln', 'conclude_step M2/R: 6 findings, 4 fixed, 2 skipped'],
  ['api', 'kiln', 'kiln', 'flow screenshot S-21 “Settings, narrow window”'],
  ['conversations', 'kiln', 'kiln', 'kiln → fixer: the import path drops EXIF dates'],
  ['agents', 'orbit', 'orbit', 'add_note N-9 “Sync slows down above 50k photos”'],
  ['builds', 'kiln', 'kiln', 'Job #340 cargo clippy --all-targets: passed in 58 s'],
  ['agents', 'kiln', 'kiln', 'record_decision D-22 “Exports keep EXIF by default”'],
  ['api', 'orbit', 'orbit', 'flow note bench.md: “LAN sync benchmark”'],
  ['system', 'poller', 'kiln', '#31 CI passing'],
  ['agents', 'orbit', 'orbit', 'add_tasks: 4 tasks to M2/S3, 6 h estimated'],
  ['api', 'orbit', 'orbit', 'flow remote --fetch target/release/orbit: exit 0'],
];

export function start() {
  const root = document.querySelector<HTMLElement>('[data-console]');
  if (!root) return;
  const $ = <T extends Element = HTMLElement>(s: string) => root.querySelector<T>(s)!;
  const icons = new Map<string, string>();
  document.querySelectorAll<HTMLTemplateElement>('template[data-icon]').forEach((t) => icons.set(t.dataset.icon!, t.innerHTML));

  const pad = (n: number) => String(n).padStart(2, '0');
  const hms = (d: Date) => `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;

  // The clock, and the log's first rows dated a few seconds apart up to now.
  const clock = $('[data-clock]');
  const tick = () => (clock.textContent = hms(new Date()));
  tick();
  const now = Date.now();
  root.querySelectorAll('[data-log] time').forEach((t, i) => (t.textContent = hms(new Date(now - (i * 11 + 3) * 1000))));
  const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
  if (reduced) return;
  setInterval(tick, 1000);

  // ---- helpers
  const text = (key: string, value: string) => {
    const el = $(`[data-t="${key}"]`);
    el.textContent = value;
    el.classList.remove('flash');
    void el.offsetWidth;
    el.classList.add('flash');
    setTimeout(() => el.classList.remove('flash'), 900);
  };
  const chip = (el: Element, state: string, label = state) => {
    el.className = `chip ${TONE[state] ?? ''}`.trim();
    el.textContent = label;
  };
  const task = (id: string, state: string) => chip($(`[data-task="${id}"]`), state);
  const desk = (name: string, s: string, live = false) => {
    const d = $(`[data-desk="${name}"]`);
    d.dataset.s = s;
    d.classList.toggle('live', live);
  };
  const count = (key: string, value: number) => {
    const b = $(`[data-c="${key}"]`);
    if (b.textContent === String(value)) return;
    b.textContent = String(value);
    b.classList.add('bump');
    setTimeout(() => b.classList.remove('bump'), 700);
  };
  const press = (sel: string) => {
    const b = $(sel);
    b.classList.add('press');
    setTimeout(() => b.classList.remove('press'), 260);
  };
  const log = (cat: Cat, who: string, proj: string, what: string) => {
    const list = $('[data-log]');
    const li = document.createElement('li');
    li.className = 'in';
    const t = document.createElement('time');
    t.textContent = hms(new Date());
    const c = document.createElement('span');
    c.className = `lcat c-${cat}`;
    c.innerHTML = `${icons.get(cat) ?? ''}<span>${LABEL[cat]}</span>`;
    const w = document.createElement('span');
    w.className = 'who';
    w.textContent = who;
    const p = document.createElement('span');
    p.className = 'proj';
    p.textContent = proj;
    const x = document.createElement('span');
    x.className = 'what';
    x.textContent = what;
    li.append(t, c, w, p, x);
    list.prepend(li);
    while (list.children.length > 9) list.lastElementChild!.remove();
  };
  const notify = (title: string, body: string) => {
    const list = $('[data-notifs]');
    const logo = list.querySelector('img')?.getAttribute('src') ?? '';
    const li = document.createElement('li');
    li.className = 'nt';
    li.innerHTML = `<img src="${logo}" alt=""><span class="ttl"><strong></strong><span class="sub"></span></span><time>now</time>`;
    li.querySelector('strong')!.textContent = title;
    li.querySelector('.sub')!.textContent = body;
    list.querySelectorAll('time').forEach((t, i) => (t.textContent = i === 0 ? '1 min' : `${i + 1} min`));
    list.prepend(li);
    while (list.children.length > 3) list.lastElementChild!.remove();
  };
  const pick = (n: number | null) =>
    root.querySelectorAll('[data-choice]').forEach((c) => c.classList.toggle('pick', Number((c as HTMLElement).dataset.choice) === n));
  const pr = (n: string, state: string | null, label?: string, sub?: string) => {
    const li = $(`[data-pr="${n}"]`);
    if (state === null) {
      li.dataset.s = 'hidden';
      return;
    }
    li.dataset.s = state === 'gone' ? 'gone' : '';
    if (state !== 'gone') chip(li.querySelector('.chip')!, state, label);
    if (sub) li.querySelector('.ttl .sub')!.textContent = sub;
  };
  const waits = (n: string, ok: boolean) => $(`[data-w="${n}"]`).classList.toggle('ok', ok);
  const unlock = (sel: string, on: boolean) => {
    const b = $(sel);
    b.classList.toggle('glow', on);
    b.classList.toggle('secondary', !on && b.hasAttribute('data-locked'));
    b.style.opacity = on || !b.hasAttribute('data-locked') ? '' : '0.55';
  };

  // ---- the story, from the moment the page shows (a question waits)
  let merged = 12;
  let builds = 6;
  const reset = () => {
    text('lumen', 'T-31 · Album sidebar');
    chip($('[data-m3]'), 'active');
    task('T-30', 'done'); task('T-31', 'doing'); task('T-32', 'todo'); task('T-33', 'todo');
    text('stepcount', '1 of 4 done');
    $('[data-meter]').style.width = '25%';
    $('[data-notifs]').replaceChildren();
    desk('answer', 'idle'); pick(null);
    pr('51', 'ready', 'ready'); pr('52', null); pr('49', 'draft');
    desk('merge', 'idle');
    waits('51', false); waits('52', false);
    $('[data-t="relnote"]').className = 'big-state warn';
    $('[data-t="relnote"]').textContent = 'waits for 2 of its PRs';
    desk('tag', 'locked'); unlock('[data-btn="tag"]', false);
    $('[data-t="tagnote"]').textContent = 'Unlocks once #52 is merged.';
    desk('try', 'idle');
    merged = 12; builds = 6;
    count('waiting', 0); count('merged', merged); count('builds', builds); count('doing', 3);
    text('bnum', 'B-6'); $('[data-t="bname"]').textContent = '2026-11-23-1718-main-3f1c2aa';
    $('[data-t="bcmd"]').innerHTML = '<span class="p">$ </span>flow run lumen 6';
  };

  const steps: [number, () => void][] = [
    [1700, () => pick(1)],
    [1500, () => {
      press('[data-btn="answer"]');
      setTimeout(() => {
        desk('answer', 'answered');
        task('T-32', 'todo');
        count('waiting', 0);
        log('you', 'owner', 'lumen', 'answered Q-8 → D-14 “The sidebar is a list of names”');
      }, 280);
    }],
    [1900, () => {
      task('T-31', 'done'); task('T-32', 'doing');
      text('stepcount', '2 of 4 done');
      $('[data-meter]').style.width = '50%';
      text('lumen', 'T-32 · Drag photos onto an album');
      log('agents', 'lumen', 'lumen', 'update_task T-31 → done, T-32 → doing');
    }],
    [2100, () => {
      pr('52', '', 'CI running', 'based on #51');
      desk('merge', 'ready');
      unlock('[data-btn="merge"]', false);
      $('[data-btn="merge"]').style.opacity = '0.55';
      $('[data-t="mergenote"]').textContent = 'Waits for CI on #52.';
      text('lumen', 'opened #52');
      log('system', 'poller', 'lumen', '#52 opened: feat: drag photos onto an album, based on #51');
    }],
    [2000, () => {
      pr('52', 'ready', 'ready', 'based on #51, merges after it');
      unlock('[data-btn="merge"]', true);
      $('[data-btn="merge"]').style.opacity = '';
      $('[data-desk="merge"]').classList.add('live');
      $('[data-t="mergenote"]').textContent = 'Merges from the bottom, stops at the first that fails.';
      notify('Something to review · lumen', 'Merge #51–#52 is ready');
      log('system', 'poller', 'lumen', '#52 CI passing: Merge #51–#52 is ready');
    }],
    [1800, () => {
      press('[data-btn="merge"]');
      setTimeout(() => {
        desk('merge', 'done');
        pr('51', 'merged'); pr('52', 'merged');
        waits('51', true); waits('52', true);
        merged += 2; count('merged', merged);
        $('[data-t="relnote"]').className = 'big-state ok';
        $('[data-t="relnote"]').textContent = 'ready to tag: every PR merged';
        desk('tag', 'ready', true); unlock('[data-btn="tag"]', true);
        $('[data-t="tagnote"]').textContent = 'Every PR it waits for is merged.';
        log('you', 'owner', 'lumen', 'Merge #51–#52: 2 of 2 merged');
      }, 280);
      setTimeout(() => { pr('51', 'gone'); pr('52', 'gone'); }, 1300);
    }],
    [1900, () => {
      press('[data-btn="tag"]');
      setTimeout(() => {
        desk('tag', 'done');
        chip($('[data-m3]'), 'closed');
        $('[data-t="relnote"]').textContent = 'tagged · M3 closed';
        log('you', 'owner', 'lumen', 'Push tag v0.4.0-m3: M3 closed');
      }, 280);
    }],
    [2300, () => {
      text('lumen', 'flow publish B-7');
      $('[data-bled]').classList.add('busy');
      const m = $('[data-bmeter]');
      m.style.transition = 'none';
      m.style.width = '0%';
      void m.offsetWidth;
      m.style.transition = 'width 1.6s ease';
      m.style.width = '100%';
      log('api', 'lumen', 'lumen', 'flow publish B-7 “Drag photos onto an album”, 1 to try');
      setTimeout(() => {
        $('[data-bled]').classList.remove('busy');
        text('bnum', 'B-7');
        $('[data-t="bname"]').textContent = '2026-11-24-1402-main-8b04d7e';
        $('[data-t="bcmd"]').innerHTML = '<span class="p">$ </span>flow run lumen 7';
        builds += 1; count('builds', builds);
      }, 1700);
    }],
    [1700, () => {
      desk('try', 'new', true);
      notify('Build ready to try · lumen', 'B-7: Drag photos onto an album');
      text('lumen', 'T-33 · Album covers');
      task('T-32', 'done'); task('T-33', 'doing');
      text('stepcount', '3 of 4 done');
      $('[data-meter]').style.width = '75%';
    }],
    [2400, () => {
      press('[data-btn="works"]');
      setTimeout(() => {
        desk('try', 'done');
        log('you', 'owner', 'lumen', 'B-7: Works on TT-9 “Drag photos onto an album”');
      }, 280);
    }],
    [3200, () => reset()],
    [2200, () => {
      text('lumen', 'asks Q-8 · Album sidebar');
      task('T-32', 'blocked');
      desk('answer', 'asked', true);
      count('waiting', 1);
      notify('New question · lumen', 'Album sidebar: a list of names or a grid of covers?');
      log('agents', 'lumen', 'lumen', 'ask_question Q-8: Album sidebar: a list of names or a grid of covers?');
    }],
  ];

  // ---- the player
  let i = 0;
  let timer = 0;
  let chatter = 0;
  let c = 0;
  let running = false;
  let wanted = true; // the Pause button
  let visible = false;
  const step = () => {
    const [ms, run] = steps[i];
    run();
    i = (i + 1) % steps.length;
    timer = window.setTimeout(step, ms);
  };
  const talk = () => {
    const [cat, who, proj, what] = CHATTER[c];
    c = (c + 1) % CHATTER.length;
    log(cat, who, proj, what);
    if (who === 'orbit') text('orbit', what.startsWith('Job') || what.startsWith('flow') ? what.split(':')[0] : 'T-12 · Sync over LAN');
    if (who === 'kiln') text('kiln', what.split(' ')[0] === 'conclude_step' ? 'M2 review concluded' : 'M2 review · fixing');
    count('doing', 3 + (c % 2));
    chatter = window.setTimeout(talk, 2300 + Math.random() * 1600);
  };
  const play = () => {
    if (running || !wanted || !visible || document.hidden) return;
    running = true;
    root.classList.remove('paused');
    timer = window.setTimeout(step, 1200);
    chatter = window.setTimeout(talk, 900);
  };
  const stop = () => {
    running = false;
    clearTimeout(timer);
    clearTimeout(chatter);
  };

  const toggle = $<HTMLButtonElement>('[data-toggle]');
  toggle.hidden = false;
  toggle.addEventListener('click', () => {
    wanted = !wanted;
    toggle.querySelector('span')!.textContent = wanted ? 'Pause' : 'Play';
    root.classList.toggle('paused', !wanted);
    if (wanted) play();
    else stop();
  });

  new IntersectionObserver(([e]) => {
    visible = e.isIntersecting;
    if (visible) play();
    else stop();
  }, { threshold: 0.15 }).observe(root);
  document.addEventListener('visibilitychange', () => (document.hidden ? stop() : play()));
}
