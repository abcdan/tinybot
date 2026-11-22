import assert from 'node:assert/strict';
import {test} from 'node:test';
import {chromium} from 'playwright';
import {describe,populate,publicUrl} from './forms.mjs';

test('public URL parsing accepts HTTPS and rejects credentials, fragments and alternate ports', () => {
    assert.equal(publicUrl('https://example.org/form').href, 'https://example.org/form');
    for (const url of ['http://example.org/', 'https://example.org:444/', 'https://user@example.org/', 'https://example.org/#private']) {
        assert.throws(() => publicUrl(url));
    }
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
