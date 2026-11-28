import assert from 'node:assert/strict';
import {access} from 'node:fs/promises';
import {test} from 'node:test';
import {setTimeout as delay} from 'node:timers/promises';
import {chromium, errors} from 'playwright';
import {captureViewport, dismissCookieDialog, recordingOptions} from './capture.mjs';
import {describe,populate,publicUrl} from './forms.mjs';
import {allowsRead, indexedUrl, operate, readableUrl, search, visit} from './server.mjs';

test('public URL parsing accepts HTTPS and rejects credentials, fragments and alternate ports', () => {
    assert.equal(publicUrl('https://example.org/form').href, 'https://example.org/form');
    for (const url of ['http://example.org/', 'https://example.org:444/', 'https://user@example.org/', 'https://example.org/#private']) {
        assert.throws(() => publicUrl(url));
    }
});

test('a website response explains challenge pages and other HTTP failures', async () => {
    const url = new URL('https://fixture.example/');
    const pageWith = (status, headers = {}) => ({
        goto: async () => ({status: () => status, headers: () => headers, ok: () => status === 200}),
        url: () => url.href,
    });
    assert.equal((await visit(pageWith(200), url)).href, url.href);
    await assert.rejects(visit(pageWith(403, {'cf-mitigated': 'challenge'}), url), /Cloudflare requires verification/);
    await assert.rejects(visit(pageWith(404), url), /returned HTTP 404/);
    const failed = error => ({goto: async () => { throw error; }});
    await assert.rejects(visit(failed(new errors.TimeoutError()), url), /took too long/);
    await assert.rejects(
        visit(failed(new Error('page.goto: net::ERR_TUNNEL_CONNECTION_FAILED at https://fixture.example/')), url),
        {message: 'The browser could not establish an HTTPS connection to the website.'}
    );
});

test('reading a website preserves its client-side route', async () => {
    const url = readableUrl('https://fixture.example/#/documentation');
    const browser = await chromium.launch({chromiumSandbox: true});
    try {
        const page = await browser.newPage();
        await page.route('https://fixture.example/**', route => route.fulfill({
            contentType: 'text/html',
            body: '<p></p><script>document.querySelector("p").textContent = location.hash</script>',
        }));
        assert.equal((await visit(page, url, {validate: readableUrl})).href, url.href);
        assert.equal(await page.locator('p').innerText(), '#/documentation');
        assert.throws(() => publicUrl(url.href));
        assert.throws(() => readableUrl('https://user@fixture.example/#/documentation'));
    } finally {
        await browser.close();
    }
});

test('readable articles skip slow assets and application shells still load', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    const article = 'A fixture article with enough text to read from the first render. '.repeat(5);
    let unrelatedScriptFinished = false;
    const fixture = {
        async newContext(settings) {
            const context = await browser.newContext(settings);
            const newPage = context.newPage.bind(context);
            context.newPage = async () => {
                const page = await newPage();
                await page.route('https://fixture.example/**', async route => {
                    const path = new URL(route.request().url()).pathname;
                    if (path === '/slow.js') {
                        await delay(1800, undefined, {ref: false});
                        if (page.isClosed()) return;
                        unrelatedScriptFinished = true;
                        return route.fulfill({contentType: 'text/javascript', body: ''});
                    }
                    if (path === '/render.js') {
                        await delay(150);
                        return route.fulfill({contentType: 'text/javascript', body:
                            'document.querySelector("main").textContent = "Hydrated primary content";',
                        });
                    }
                    const body = path === '/static'
                        ? '<main>' + article + '</main><script async src="/slow.js"></script>'
                        : '<main aria-busy="true">' + 'Loading placeholder. '.repeat(15) +
                            '</main><script async src="/render.js"></script>';
                    return route.fulfill({contentType: 'text/html', body});
                });
                return page;
            };
            return context;
        },
    };
    try {
        const page = await operate(fixture, '/browse', {url: 'https://fixture.example/static'});
        assert.equal(page.text.trim(), article.trim());
        assert.equal(unrelatedScriptFinished, false);
        const hydrated = await operate(fixture, '/browse', {url: 'https://fixture.example/client'});
        assert.equal(hydrated.text, 'Hydrated primary content');
    } finally {
        await browser.close();
    }
});

test('rendering allows page resources while rejecting write methods', () => {
    assert.equal(allowsRead('GET', 'document', false), true);
    assert.equal(allowsRead('GET', 'script', false), false);
    assert.equal(allowsRead('GET', 'script', true), true);
    assert.equal(allowsRead('GET', 'fetch', true), true);
    assert.equal(allowsRead('HEAD', 'xhr', true), true);
    assert.equal(allowsRead('POST', 'fetch', true), false);
    assert.equal(allowsRead('PUT', 'xhr', true), false);
    assert.equal(allowsRead('GET', 'websocket', true), false);
});

test('captures allow visual assets but keep browser requests read-only', () => {
    assert.equal(allowsRead('GET', 'image', true), false);
    for (const asset of ['image', 'font', 'media']) {
        assert.equal(allowsRead('GET', asset, true, true), true);
        assert.equal(allowsRead('POST', asset, true, true), false);
    }
    assert.equal(allowsRead('GET', 'websocket', true, true), false);
    assert.deepEqual(recordingOptions({}), {seconds: 5, scroll: false});
    for (const duration of [0, 1, 11, 2.5, '5']) {
        assert.throws(() => recordingOptions({duration_seconds: duration}), /2 to 10 seconds/);
    }
    assert.throws(() => recordingOptions({scroll: 'yes'}), /boolean scroll/);
    assert.equal(dismissCookieDialog({}), true);
    assert.equal(dismissCookieDialog({dismiss_cookie_dialog: false}), false);
    assert.throws(() => dismissCookieDialog({dismiss_cookie_dialog: 'yes'}), /boolean dismiss_cookie_dialog/);
});

test('captures a PNG and a playable MP4, and deletes recording files on success, error, or cancellation', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    const directories = [];
    const closedDirectories = [];
    let scrollRequests = 0;
    const abort = new AbortController();
    const fixture = {
        async newContext(settings) {
            const context = await browser.newContext(settings);
            const directory = settings.recordVideo?.dir;
            if (directory) {
                directories.push(directory);
                const close = context.close.bind(context);
                context.close = async () => {
                    await close();
                    await access(directory);
                    closedDirectories.push(directory);
                };
            }
            const newPage = context.newPage.bind(context);
            context.newPage = async () => {
                const page = await newPage();
                await page.route('https://fixture.example/**', async request => {
                    const path = new URL(request.request().url()).pathname;
                    if (path === '/cancel') {
                        abort.abort();
                        return;
                    }
                    if (path === '/missing') {
                        return request.fulfill({status: 404, contentType: 'text/html', body: '<p>Not found</p>'});
                    }
                    if (path === '/scroll') {
                        scrollRequests += 1;
                        return request.fulfill({status: 204});
                    }
                    return request.fulfill({contentType: 'text/html', body:
                        '<body style="min-height:3000px;background:linear-gradient(orange,blue)">' +
                        '<h1>Capture fixture</h1><script>addEventListener("scroll",()=>fetch("/scroll"),{once:true})</script></body>',
                    });
                });
                return page;
            };
            return context;
        },
    };
    try {
        const {bytes: image, cookieDialog} = await operate(fixture, '/screenshot', {url: 'https://fixture.example/'});
        assert.equal(cookieDialog, null);
        assert.equal(image.subarray(0, 8).toString('hex'), '89504e470d0a1a0a');
        assert.equal(image.readUInt32BE(16), captureViewport.width);
        assert.equal(image.readUInt32BE(20), captureViewport.height);
        assert.deepEqual(directories, []);

        const {bytes: clip} = await operate(fixture, '/record', {url: 'https://fixture.example/', duration_seconds: 2, scroll: true});
        assert.equal(clip.subarray(4, 8).toString(), 'ftyp');
        assert.ok(clip.includes(Buffer.from('avc1')));
        const movie = clip.indexOf(Buffer.from('moov'));
        assert.ok(movie > 0 && movie < clip.indexOf(Buffer.from('mdat')));
        assert.ok(scrollRequests > 0);
        await assert.rejects(access(directories[0]), {code: 'ENOENT'});

        await assert.rejects(operate(fixture, '/record', {url: 'https://fixture.example/missing'}), /HTTP 404/);
        await assert.rejects(access(directories[1]), {code: 'ENOENT'});

        await assert.rejects(operate(fixture, '/record', {url: 'https://fixture.example/cancel'}, abort.signal));
        await assert.rejects(access(directories[2]), {code: 'ENOENT'});
        assert.deepEqual(closedDirectories, directories);
    } finally {
        await browser.close();
    }
});

test('screenshot clicks functional-only in a cookie dialog and can keep the dialog when requested', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    const clicks = [];
    const button = (label, effect) =>
        '<button onclick="fetch(\'/choice/' + effect + '\'); this.closest(\'section\').remove()">' + label + '</button>';
    const functional = button('Functional only', 'functional');
    const accept = button('Allow all', 'allow-all');
    const markup = {
        '/': '<div id="app"><section><h2>Approve submissions</h2>' + button('Reject all', 'unrelated') + '</section>' +
            '<section style="position:fixed;inset:20px;background:white">' +
            '<h2>Cookies</h2><p>We use cookies to improve your browsing experience.</p>' + functional + accept + '</section></div>',
        '/blocked': '<section role="dialog"><h2>Cookies</h2>' + accept + '</section>',
        '/form': '<section role="dialog"><h2>Cookies</h2>' +
            '<form action="/choice/form"><button>Reject all</button></form></section>',
        '/framed': '<iframe src="https://consent.fixture.example/"></iframe>',
        '/offer': '<section role="dialog"><h2>Special offer</h2><p>Get a cookie with your membership.</p>' +
            button('No thanks', 'unrelated') + '</section>' +
            '<footer class="cookie-settings"><button aria-label="Cookie settings">Cookie settings</button></footer>',
        '/delayed-popup': `
            <style>
                .cookie-warning-modal { position: fixed; top: 24px; left: 24px; width: 0; height: 0; }
                .cookie-warning-modal__inner { display: none; width: 330px; padding: 24px; background: #eee; }
                .show .cookie-warning-modal__inner { display: block; }
                .btn-cookie { cursor: pointer; padding: 8px; }
            </style>
            <h1>Public page</h1>
            <div class="btn-cookie" onclick="fetch('/choice/unrelated')">Functional only</div>
            <div id="cookie" class="cookie-warning-modal cookie-warning-modal--popup">
                <div class="cookie-warning-modal__inner cookie">
                    <div class="cookie__body__title">Cookies</div>
                    <p>Choose whether to allow optional website cookies.</p>
                    <div class="cookie__footer-block">
                        <div class="btn-cookie" aria-label="Allow all" onclick="fetch('/choice/unrelated')">Functional only</div>
                        <div class="btn-cookie btn-cookie--ghost" id="accept-functional">Functional only</div>
                        <div class="btn-cookie" id="accept-all" onclick="fetch('/choice/allow-all')">Allow all</div>
                    </div>
                </div>
            </div>
            <script>
                const dialog = document.querySelector('#cookie');
                document.querySelector('#accept-functional').addEventListener('click', () => {
                    fetch('/choice/late-functional');
                    dialog.classList.remove('show');
                });
                document.addEventListener('DOMContentLoaded', () => {
                    setTimeout(() => dialog.classList.add('show'), 1000);
                });
            </script>`,
    };
    const fixture = {
        async newContext(settings) {
            const context = await browser.newContext(settings);
            const newPage = context.newPage.bind(context);
            context.newPage = async () => {
                const page = await newPage();
                await page.route(url => ['fixture.example', 'consent.fixture.example'].includes(url.hostname), request => {
                    const url = new URL(request.request().url());
                    if (url.pathname.startsWith('/choice/')) {
                        clicks.push(url.pathname);
                        return request.fulfill({status: 204});
                    }
                    return request.fulfill({contentType: 'text/html', body: url.hostname === 'consent.fixture.example'
                        ? '<section role="dialog" hidden><h2>Cookies</h2>' + button('Alleen noodzakelijke cookies', 'necessary') +
                            '</section><script>setTimeout(()=>document.querySelector("section").hidden=false, 150)</script>'
                        : markup[url.pathname]});
                });
                return page;
            };
            return context;
        },
    };
    try {
        const kept = await operate(fixture, '/screenshot', {url: 'https://fixture.example/', dismiss_cookie_dialog: false});
        assert.equal(kept.cookieDialog, 'visible');
        assert.deepEqual(clicks, []);

        const dismissed = await operate(fixture, '/screenshot', {url: 'https://fixture.example/'});
        assert.equal(dismissed.cookieDialog, 'dismissed');
        assert.notDeepEqual(dismissed.bytes, kept.bytes);
        assert.deepEqual(clicks, ['/choice/functional']);

        const fromIframe = await operate(fixture, '/record', {url: 'https://fixture.example/framed', duration_seconds: 2});
        assert.equal(fromIframe.cookieDialog, 'dismissed');
        assert.equal(fromIframe.bytes.subarray(4, 8).toString(), 'ftyp');
        assert.deepEqual(clicks, ['/choice/functional', '/choice/necessary']);

        for (const path of ['/blocked', '/form']) {
            const blocked = await operate(fixture, '/screenshot', {url: 'https://fixture.example' + path});
            assert.equal(blocked.cookieDialog, 'visible', path);
        }
        const offer = await operate(fixture, '/screenshot', {url: 'https://fixture.example/offer'});
        assert.equal(offer.cookieDialog, null);
        assert.deepEqual(clicks, ['/choice/functional', '/choice/necessary']);

        const late = {url: 'https://fixture.example/delayed-popup'};
        const visiblePopup = await operate(fixture, '/screenshot', {...late, dismiss_cookie_dialog: false});
        assert.equal(visiblePopup.cookieDialog, 'visible');
        assert.deepEqual(clicks, ['/choice/functional', '/choice/necessary']);
        const dismissedPopup = await operate(fixture, '/screenshot', late);
        assert.equal(dismissedPopup.cookieDialog, 'dismissed');
        assert.notDeepEqual(dismissedPopup.bytes, visiblePopup.bytes);
        assert.deepEqual(clicks, ['/choice/functional', '/choice/necessary', '/choice/late-functional']);
    } finally {
        await browser.close();
    }
});

test('indexed links use the published website rather than a search provider redirect', () => {
    const target = 'https://core.telegram.org/bots/api';
    const bing = `https://www.bing.com/ck/a?u=a1${Buffer.from(target).toString('base64url')}`;
    const google = `https://www.google.com/url?q=${encodeURIComponent(target)}`;
    assert.equal(indexedUrl(bing, 'www.bing.com'), target);
    assert.equal(indexedUrl(google, 'www.google.com'), target);
    assert.throws(() => indexedUrl('https://www.bing.com/ck/a?u=bad', 'www.bing.com'));
    assert.throws(() => indexedUrl('https://www.google.com/url?q=http://example.org', 'www.google.com'));
});

function searchFixture(bing, google = []) {
    const providers = {'www.bing.com': bing, 'www.google.com': google};
    const visits = [];
    let url;
    const page = {
        goto: async href => {
            url = new URL(href);
            visits.push(url.hostname);
            return {status: () => 200, headers: () => ({}), ok: () => true};
        },
        url: () => url.href,
        locator: () => ({evaluateAll: async () => providers[url.hostname]}),
    };
    return {page, visits};
}

test('a literal domain search prefers a first-party result to an indexed mention', async () => {
    const mention = {url: 'https://directory.example/1', title: 'Studio profile', description: 'Website: lngzl.nl.'};
    const irrelevant = {url: 'https://www.google.nl/index.html', title: 'Google', description: 'Public search'};
    const official = {url: 'https://lngzl.nl/', title: 'Studio', description: ''};
    const {page, visits} = searchFixture([irrelevant, mention], [official]);
    assert.deepEqual(await search(page, ' LNGZL.nl '), {results: [official]});
    assert.deepEqual(visits, ['www.bing.com', 'www.google.com']);
    const mentionsOnly = searchFixture([irrelevant, mention]);
    assert.deepEqual(await search(mentionsOnly.page, 'lngzl.nl'), {results: [mention]});
});

test('a site-scoped search rejects unrelated hosts and falls back to the next provider', async () => {
    const deceptive = {url: 'https://lngzl.nl.other.example/', title: 'Directory', description: 'This says lngzl.nl'};
    const official = {url: 'https://docs.lngzl.nl/hosting', title: 'Hosting', description: ''};
    const {page, visits} = searchFixture([deceptive], [deceptive, official]);
    assert.deepEqual(await search(page, 'site:lngzl.nl hosting'), {results: [official]});
    assert.deepEqual(visits, ['www.bing.com', 'www.google.com']);
    const noMatch = searchFixture([deceptive]);
    await assert.rejects(search(noMatch.page, 'site:lngzl.nl'), /did not return a result for this domain/);
    const generic = searchFixture([deceptive], [official]);
    assert.deepEqual(await search(generic.page, 'digital studios'), {results: [deceptive]});
    assert.deepEqual(generic.visits, ['www.bing.com']);
});

test('a native form can be inspected and filled and a changed target invalidates the approval', async () => {
    const browser = await chromium.launch({chromiumSandbox: true});
    const context = await browser.newContext({javaScriptEnabled: false});
    const page = await context.newPage();
    let target = '/send';
    const posts = [];
    await page.route('https://fixture.example/**', request => {
        const method = request.request().method();
        if (method === 'POST') {
            posts.push(request.request().postData());
            return request.fulfill({status: 204});
        }
        return request.fulfill({contentType: 'text/html', body:
            '<form method="post" action="' + target + '">' +
            '<input type="hidden" name="nonce" value="test">' +
            '<label>Contact email<input name="contact" type="email" required></label>' +
            '<label>Topic<select name="topic"><option value="sales">Sales</option></select></label></form>'
        });
    });
    try {
        await page.goto('https://fixture.example/form');
        const pinned = await describe(page, 0);
        assert.deepEqual(pinned.fields.map(({name, type}) => [name, type]), [['contact', 'email'], ['topic', 'select']]);
        await assert.rejects(populate(page, pinned, {contact: 'not-an-email'}));
        await page.goto('https://fixture.example/form');
        await populate(page, pinned, {contact: 'test@example.org', topic: 'sales'});
        const response = page.waitForResponse(event => event.request().method() === 'POST');
        await page.evaluate(() => HTMLFormElement.prototype.submit.call(document.forms[0]));
        await response;
        assert.deepEqual(posts, ['nonce=test&contact=test%40example.org&topic=sales']);
        target = '/different';
        await page.goto('https://fixture.example/form');
        await assert.rejects(populate(page, pinned, {contact: 'test@example.org'}), /changed the form/);
        assert.equal(posts.length, 1);
    } finally {
        await browser.close();
    }
});
