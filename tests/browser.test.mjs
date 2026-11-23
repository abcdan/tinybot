import assert from 'node:assert/strict';
import {test} from 'node:test';
import {chromium, errors} from 'playwright';
import {describe,populate,publicUrl} from './forms.mjs';
import {allowsRead, indexedUrl, readableUrl, search, visit} from './server.mjs';

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
