// Real-browser check of auth-web. Started by scripts/browser-check.sh, which provides the environment below.
// Prints PASS/FAIL lines and exits non-zero when anything failed.
import { chromium } from 'playwright-core';
import http from 'node:http';
import fs from 'node:fs';

const { WEB, SVC, PROXY_PORT, SEED, WEB_LOG, SCREENS } = process.env;
const ADMIN_PW = 'browser-check-password-1';
const USER_PW = 'browser-check-password-2';
const SIZES = [[360, 740], [1440, 900]];
const THEMES = ['light', 'dark'];

let fails = 0;
function check(name, ok, detail = '') {
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok || !detail ? '' : ` (${detail})`}`);
  if (!ok) fails++;
}

// ---- recording proxy: auth-web -> here -> auth-service. Keeps the headers of every API call.
const calls = [];
const upstream = new URL(SVC);
const proxy = http.createServer((req, res) => {
  calls.push({ method: req.method, path: req.url, xff: req.headers['x-forwarded-for'], ua: req.headers['x-client-user-agent'] });
  const out = http.request(
    { host: upstream.hostname, port: upstream.port, method: req.method, path: req.url, headers: req.headers },
    (r) => { res.writeHead(r.statusCode, r.headers); r.pipe(res); });
  out.on('error', () => { res.writeHead(502); res.end(); });
  req.pipe(out);
});
await new Promise((ok) => proxy.listen(Number(PROXY_PORT), '127.0.0.1', ok));

// ---- browser plumbing
const browser = await chromium.launch({ channel: 'chrome', headless: true });
const problems = []; // CSP violations, console errors and page errors, from every page of every context
let expected = []; // console-error patterns that the current step causes on purpose

async function newContext(label) {
  const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  await ctx.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: WEB });
  await ctx.exposeBinding('__cspViolation', (_src, v) => problems.push(`${label}: CSP ${v}`));
  await ctx.addInitScript(() => document.addEventListener('securitypolicyviolation', (e) =>
    window.__cspViolation(`${e.violatedDirective} blocked ${e.blockedURI || 'inline'} on ${location.pathname}`)));
  const page = await ctx.newPage();
  page.on('console', (m) => {
    if (m.type() !== 'error') return;
    if (expected.some((re) => re.test(m.text()))) return;
    problems.push(`${label}: console error on ${new URL(page.url()).pathname}: ${m.text()}`);
  });
  page.on('pageerror', (e) => problems.push(`${label}: page error on ${new URL(page.url()).pathname}: ${e.message}`));
  return page;
}

const path = (page) => new URL(page.url()).pathname;
// Interactive pages are usable once the circuit is up: Blazor then drops the prerender marker comments' state,
// which is not observable, so wait for the websocket instead.
async function gotoInteractive(page, url) {
  const ws = page.waitForEvent('websocket', { timeout: 15000 }).catch(() => null);
  await page.goto(WEB + url);
  await ws;
  await page.waitForLoadState('networkidle');
}
async function signIn(page, login, password) {
  await page.goto(`${WEB}/signin`);
  await page.fill('#login', login);
  await page.fill('#password', password);
  await page.click('form[action^="/signin"] button[type=submit]');
  await page.waitForLoadState('networkidle');
}
async function changePassword(page, current, next) {
  await page.fill('#current', current);
  await page.fill('#new', next);
  await page.fill('#confirm', next);
  await page.click('button[type=submit]');
  await page.waitForLoadState('networkidle');
}
const noHScroll = (page) => page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth);

fs.mkdirSync(SCREENS, { recursive: true });
async function shot(page, name, { width, height } = {}, theme) {
  if (width) await page.setViewportSize({ width, height });
  if (theme) await page.emulateMedia({ colorScheme: theme });
  await page.waitForTimeout(150); // let transitions settle
  await page.screenshot({ path: `${SCREENS}/${name}.png`, fullPage: true });
}
// Every size and theme of one page: no horizontal scroll, and a screenshot of each.
async function sweep(page, url, name, interactive) {
  for (const [width, height] of SIZES) {
    await page.setViewportSize({ width, height });
    if (interactive) await gotoInteractive(page, url); else await page.goto(WEB + url);
    check(`10 no horizontal scroll on ${url} at ${width}x${height}`, await noHScroll(page),
      await page.evaluate(() => `${document.documentElement.scrollWidth} > ${document.documentElement.clientWidth}`));
    for (const theme of THEMES) await shot(page, `${name}-${width}-${theme}`, {}, theme);
  }
  // In between the two: tables switch from stacked rows to columns around here.
  for (const width of [768, 1024]) {
    await page.setViewportSize({ width, height: 900 });
    check(`10 no horizontal scroll on ${url} at ${width} wide`, await noHScroll(page));
  }
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.emulateMedia({ colorScheme: 'light' });
}

try {
  // ---- 1 sign in as the seeded admin, forced change, /account
  const admin = await newContext('admin');
  await signIn(admin, 'wiertmir', SEED);
  check('1a seeded admin sign-in is sent to /change-password', path(admin) === '/change-password', path(admin));
  await shot(admin, 'change-password-1440-light');
  await changePassword(admin, SEED, ADMIN_PW);
  check('1b after the forced change the admin lands on /account', path(admin) === '/account', path(admin));

  // ---- 3 interactive circuit: edit the display name, reload
  await gotoInteractive(admin, '/account');
  const before = calls.length;
  await admin.fill('#display-name', 'Mirek W');
  await admin.click('button:has-text("Save display name")');
  await admin.getByText('Saved.').waitFor({ timeout: 10000 }).catch(() => {});
  check('3a saving the display name over the circuit shows "Saved."', await admin.getByText('Saved.').isVisible());
  await gotoInteractive(admin, '/account');
  check('3b the new display name is shown after a reload', (await admin.inputValue('#display-name')) === 'Mirek W');

  // ---- 4 client address on calls made from the circuit
  const patch = calls.slice(before).find((c) => c.method === 'PATCH' && c.path === '/api/me');
  check('4a auth-web forwards a real client IP on a circuit call', !!patch && /^(127\.0\.0\.1|::1)$/.test(patch.xff ?? ''),
    patch ? `X-Forwarded-For: ${patch.xff}` : 'no PATCH /api/me seen');
  check('4b the circuit call carries the browser User-Agent', !!patch && /Chrome/.test(patch.ua ?? ''), patch?.ua);
  check('4c auth-web log has no "Circuit opened without a client address"',
    !fs.readFileSync(WEB_LOG, 'utf8').includes('Circuit opened without a client address'));

  // ---- 5 app passwords: shown once, copy, gone after dismiss and reload
  await gotoInteractive(admin, '/account/app-passwords');
  await shot(admin, 'app-passwords-empty-1440-light');
  await admin.fill('#label', 'Phone calendar');
  await admin.click('button:has-text("Create app password")');
  const code = admin.locator('.secret code');
  await code.waitFor({ timeout: 10000 });
  const secret = (await code.textContent()).trim();
  check('5a the secret panel shows an app password of the documented shape', /^[a-z]{4}(-[a-z]{4}){3}$/.test(secret));
  for (const [width, height] of SIZES)
    for (const theme of THEMES) await shot(admin, `secret-panel-${width}-${theme}`, { width, height }, theme);
  await admin.setViewportSize({ width: 1440, height: 900 });
  await admin.emulateMedia({ colorScheme: 'light' });
  await admin.click('.secret button:has-text("Copy")');
  await admin.getByText('Copied.').waitFor({ timeout: 5000 }).catch(() => {});
  check('5b Copy puts the value on the clipboard', (await admin.evaluate(() => navigator.clipboard.readText())) === secret);
  await admin.click('.secret button:has-text("I\'ve saved it")');
  await admin.locator('.secret').waitFor({ state: 'detached', timeout: 5000 }).catch(() => {});
  check('5c after dismissing, the value is no longer in the DOM', !(await admin.content()).includes(secret));
  await gotoInteractive(admin, '/account/app-passwords');
  check('5d after a reload the value is not in the page', !(await admin.content()).includes(secret));

  // ---- 6 confirm dialog
  await admin.fill('#label', 'Old laptop');
  await admin.click('button:has-text("Create app password")');
  await admin.click('.secret button:has-text("I\'ve saved it")');
  const row = admin.locator('tbody tr', { hasText: 'Old laptop' });
  const del = row.locator('button', { hasText: 'Delete' });
  await del.click();
  await admin.locator('dialog[open]').waitFor({ timeout: 5000 }).catch(() => {});
  const dlg = await admin.evaluate(() => {
    const d = document.querySelector('dialog[open]:not(#components-reconnect-modal)');
    const a = document.activeElement;
    return d && { modal: d.matches(':modal'), inside: d.contains(a), focused: a.textContent.trim() };
  });
  check('6a Delete opens a modal <dialog>', !!dlg && dlg.modal);
  check('6b focus is inside the dialog and not on the destructive button', !!dlg && dlg.inside && dlg.focused === 'Cancel', dlg?.focused);
  for (const [width, height] of SIZES)
    for (const theme of THEMES) await shot(admin, `confirm-dialog-${width}-${theme}`, { width, height }, theme);
  await admin.setViewportSize({ width: 1440, height: 900 });
  await admin.emulateMedia({ colorScheme: 'light' });
  await admin.keyboard.press('Escape');
  await admin.locator('dialog[open]').waitFor({ state: 'detached', timeout: 5000 }).catch(() => {});
  check('6c Escape closes the dialog without deleting', (await admin.locator('dialog[open]').count()) === 0 && (await row.count()) === 1);
  check('6d focus returns to the button that opened it',
    await del.evaluate((b) => b === document.activeElement));
  await del.click();
  await admin.click('dialog[open] button:has-text("Delete")');
  await row.waitFor({ state: 'detached', timeout: 5000 }).catch(() => {});
  check('6e confirming deletes the item', (await row.count()) === 0 && (await admin.locator('tbody tr').count()) === 1);

  // ---- 7 admin creates a user; that user is not an admin
  await gotoInteractive(admin, '/admin/users');
  await admin.fill('#new-username', 'ania');
  await admin.fill('#new-email', 'ania@example.test');
  await admin.fill('#new-display', 'Ania');
  await admin.click('button:has-text("Create user")');
  await admin.locator('.secret code').waitFor({ timeout: 10000 });
  const temp = (await admin.locator('.secret code').textContent()).trim();
  check('7a creating a user shows a temporary password once', temp.length >= 12);
  await shot(admin, 'admin-users-secret-1440-light');
  await admin.click('.secret button:has-text("I\'ve saved it")');

  const user = await newContext('user');
  await signIn(user, 'ania', temp);
  check('7b the new user signs in with the temporary password and must change it', path(user) === '/change-password', path(user));
  await changePassword(user, temp, USER_PW);
  check('7c after the change the new user lands on /account', path(user) === '/account', path(user));
  check('7d the navigation of a non-admin has no Users link', (await user.locator('a[href="/admin/users"]').count()) === 0);
  expected = [/status of 403/];
  await user.goto(`${WEB}/admin/users`);
  check('7e /admin/users by direct load: "Not authorised"',
    (await user.locator('h1').textContent()) === 'Not authorised' && (await user.getByText('Create a user').count()) === 0);
  await shot(user, 'not-authorised-1440-light');
  // A link the app did not render, followed after the app is loaded: Blazor's enhanced navigation handles it.
  await gotoInteractive(user, '/account');
  await user.evaluate(() => {
    const a = Object.assign(document.createElement('a'), { href: '/admin/users', textContent: 'typed', id: 'typed' });
    document.querySelector('main').append(a);
  });
  await user.click('#typed');
  await user.locator('h1', { hasText: 'Not authorised' }).waitFor({ timeout: 5000 }).catch(() => {});
  check('7f /admin/users by in-app navigation: "Not authorised"',
    (await user.locator('h1').textContent()) === 'Not authorised' && (await user.getByText('Create a user').count()) === 0,
    await user.locator('h1').textContent());
  await user.waitForLoadState('networkidle');
  expected = [];

  // ---- 9 keyboard only on /signin
  const anon = await newContext('anon');
  await anon.goto(`${WEB}/signin`);
  const stops = [];
  for (let i = 0; i < 12 && stops.length < 3; i++) {
    await anon.keyboard.press('Tab');
    const s = await anon.evaluate(() => {
      const a = document.activeElement, cs = getComputedStyle(a);
      return { id: a.id || `${a.tagName.toLowerCase()}:${a.textContent.trim()}`, form: !!a.closest('form[action^="/signin"]'),
        visible: a.matches(':focus-visible') && (cs.outlineStyle !== 'none' || cs.boxShadow !== 'none') };
    });
    if (s.form) stops.push(s);
  }
  check('9a Tab reaches login, password, submit in that order', stops.map((s) => s.id).join() === 'login,password,button:Sign in',
    stops.map((s) => s.id).join());
  check('9b each of them shows a visible focus indicator', stops.length === 3 && stops.every((s) => s.visible));
  await shot(anon, 'signin-focus-1440-light');

  // ---- 10, 11 signed-out pages: no horizontal scroll, screenshots
  for (const p of ['signin', 'signup', 'forgot']) await sweep(anon, `/${p}`, p, false);
  // error states, for the eye
  await anon.setViewportSize({ width: 360, height: 740 });
  await signIn(anon, 'wiertmir', 'not-the-password');
  for (const theme of THEMES) await shot(anon, `signin-error-360-${theme}`, {}, theme);
  await anon.goto(`${WEB}/signup`);
  await anon.fill('#username', 'wiertmir');
  await anon.fill('#email', 'someone@example.test');
  await anon.fill('#password', 'long-enough-password');
  await anon.fill('#confirm', 'a-different-password');
  await anon.click('button[type=submit]');
  await anon.waitForLoadState('networkidle');
  for (const theme of THEMES) await shot(anon, `signup-error-360-${theme}`, {}, theme);
  await anon.goto(`${WEB}/reset`);
  await shot(anon, 'reset-expired-360-light', {}, 'light');
  await anon.goto(`${WEB}/verify?token=abc`);
  await shot(anon, 'verify-360-light');

  // ---- 8 sessions: a second admin browser, then "Sign out everywhere else"
  const admin2 = await newContext('admin2');
  await signIn(admin2, 'wiertmir', ADMIN_PW);
  await gotoInteractive(admin2, '/account');
  await gotoInteractive(admin, '/account/security');
  const sessions = admin.locator('section[aria-labelledby="sessions-h"] tbody tr');
  await sessions.first().waitFor({ timeout: 10000 });
  check('8a the admin sees two sessions', (await sessions.count()) === 2, `${await sessions.count()}`);
  const ips = await sessions.locator('td').first().textContent();
  check('8b a session row shows a real IP address', /^(127\.0\.0\.1|::1)$/.test(ips.trim()), ips);

  // ---- 10, 11 signed-in pages (while the lists have content)
  for (const [url, name] of [['/account', 'account'], ['/account/security', 'account-security'],
    ['/account/app-passwords', 'account-app-passwords'], ['/admin/users', 'admin-users']])
    await sweep(admin, url, name, true);

  // for the eye: the link-confirmation panel (nothing is confirmed) and the reconnect dialog in its first state
  await gotoInteractive(admin, '/account/security?link_ticket=not-a-real-ticket');
  await shot(admin, 'link-confirm-1440-light');
  await shot(admin, 'link-confirm-360-dark', { width: 360, height: 740 }, 'dark');
  await admin.click('.decision button:has-text("Cancel")');
  await gotoInteractive(admin, '/account/security');
  await admin.evaluate(() => {
    const d = document.getElementById('components-reconnect-modal');
    d.classList.add('components-reconnect-show');
    d.showModal();
  });
  await shot(admin, 'reconnect-360-dark');
  await shot(admin, 'reconnect-1440-light', { width: 1440, height: 900 }, 'light');

  await gotoInteractive(admin, '/account/security');
  await admin.click('button:has-text("Sign out everywhere else")');
  await admin.click('dialog[open] button:has-text("Sign out everywhere else")');
  await admin.getByText('Signed out everywhere else.').waitFor({ timeout: 10000 }).catch(() => {});
  check('8c "Sign out everywhere else" leaves exactly one session', (await sessions.count()) === 1, `${await sessions.count()}`);
  expected = [/status of 401/];
  await admin2.fill('#display-name', 'Should not save');
  await admin2.click('button:has-text("Save display name")');
  await admin2.waitForURL(/\/signin/, { timeout: 10000 }).catch(() => {});
  check('8d the other browser is sent to /signin on its next interaction', path(admin2) === '/signin', path(admin2));
  const cookies = await admin2.context().cookies();
  check('8e and its session cookie is cleared', !cookies.some((c) => c.name === 'me_auth'));
  await admin2.waitForLoadState('networkidle');
  expected = [];

  // ---- 2 nothing was blocked or logged as an error anywhere above
  check('2 no Content-Security-Policy violations, console errors or page errors on any page', problems.length === 0);
  for (const p of problems) console.log(`      ${p}`);
} catch (e) {
  check('the check ran to the end', false, e.message.split('\n')[0]);
  for (const p of problems) console.log(`      ${p}`);
} finally {
  await browser.close();
  proxy.close();
  proxy.closeAllConnections();
}

console.log(fails === 0 ? '\nALL PASSED' : `\n${fails} FAILED`);
console.log(`screenshots: ${SCREENS}`);
process.exit(fails === 0 ? 0 : 1);
