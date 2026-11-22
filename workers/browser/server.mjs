import { InputError, requireInput, publicUrl, describe, populate } from './forms.mjs';
import { chmod, unlink } from 'node:fs/promises';
import { createServer } from 'node:http';
import { pathToFileURL } from 'node:url';
import { chromium, errors } from 'playwright';

const LIMIT = 96_000;
const socketPath = '/browser/browser.sock';
const readResources = new Set(['document', 'script', 'stylesheet', 'fetch', 'xhr']);

export function allowsRead(method, resource, render) {
    return (method === 'GET' || method === 'HEAD') &&
        (resource === 'document' || (render && readResources.has(resource)));
}

export async function visit(page, url, waitUntil = 'domcontentloaded') {
    const response = await page.goto(url.href, {waitUntil});
    requireInput(response, 'The website did not return an HTTPS response.');
    requireInput(response.headers()['cf-mitigated'] !== 'challenge',
        'Cloudflare requires verification before this website can be read.');
    requireInput(response.ok(), `The website returned HTTP ${response.status()}.`);
    return publicUrl(page.url());
}

async function operate(browser, route, data) {
    const render = route === '/browse';
    const context = await browser.newContext({javaScriptEnabled: render, serviceWorkers: 'block', acceptDownloads: false});
    context.setDefaultTimeout(8000);
    context.setDefaultNavigationTimeout(12000);
    let permit;
    let posted = false;
    let postStatus;
    try {
        const page = await context.newPage();
        await context.routeWebSocket('**/*', socket => socket.close());
        await context.route('**/*', async intercepted => {
            const request = intercepted.request();
            let url;
            try { url = publicUrl(request.url()); }
            catch { await intercepted.abort(); return; }
            if (permit) {
                if (!posted && request.method() === 'POST' && request.isNavigationRequest() && url.href === permit) {
                    posted = true;
                    try {
                        const response = await intercepted.fetch({maxRedirects: 0, timeout: 8000});
                        postStatus = response.status();
                        await response.dispose();
                        await intercepted.fulfill({status: 204});
                    } catch { await intercepted.abort(); }
                } else await intercepted.abort();
                return;
            }
            if (allowsRead(request.method(), request.resourceType(), render)) {
                await intercepted.continue();
            } else await intercepted.abort();
        });
        if (route === '/search') {
            requireInput(typeof data.query === 'string' && data.query.length <= 240, 'Invalid search query.');
            const url = new URL('https://www.google.com/search');
            url.searchParams.set('q', data.query);
            await visit(page, url);
            const results = await page.evaluate(() => [...document.querySelectorAll('a h3')].slice(0, 18).map(title => {
                const link = new URL(title.closest('a').href);
                return {title: title.textContent.trim().slice(0, 150),
                    url: link.pathname === '/url' ? link.searchParams.get('q') || link.searchParams.get('url') : link.href};
            }));
            const publicResults = results.filter(result => {
                try { return !/(^|\.)google\.com$/.test(publicUrl(result.url).hostname); }
                catch { return false; }
            }).slice(0, 8);
            requireInput(publicResults.length, 'The web search did not return public results.');
            return {results: publicResults};
        }
        if (route === '/browse') {
            await visit(page, publicUrl(data.url), 'commit');
            await page.waitForLoadState('domcontentloaded');
            try { await page.waitForLoadState('load', {timeout: 3000}); }
            catch (error) { if (!(error instanceof errors.TimeoutError)) throw error; }
            const content = await page.locator('body').evaluate(body => ({
                title: document.title.slice(0, 200),
                text: body.innerText.slice(0, 6900),
                links: [...body.querySelectorAll('a[href]')].slice(0, 60)
                    .filter(anchor => typeof anchor.href === 'string')
                    .map(anchor => ({text: anchor.textContent.trim().slice(0, 90), url: anchor.href.slice(0, 500)}))
                    .filter(anchor => anchor.text && anchor.url.startsWith('https://')).slice(0, 16),
            }));
            return {url: publicUrl(page.url()).href, ...content};
        }
        if (route === '/inspect') {
            const index = data.index ?? 0;
            requireInput(Number.isInteger(index) && index >= 0 && index <= 40, 'Choose a valid form position.');
            await visit(page, publicUrl(data.url));
            return describe(page, index);
        }
        requireInput(route === '/submit' && data.form, 'Invalid browser operation.');
        const confirmed = data.form;
        await visit(page, publicUrl(confirmed.page));
        const observed = await populate(page, confirmed, data.values);
        requireInput(page.url() === observed.page, 'The site navigated away from the confirmed form.');
        permit = observed.destination;
        const response = page.waitForResponse(r => r.request().method() === 'POST' && r.url() === permit);
        // Keep the internal promise observed even if a failed navigation detaches the evaluator.
        const submit = page.evaluate(index => HTMLFormElement.prototype.submit.call(document.forms[index]), observed.index);
        await Promise.all([response, submit.catch(() => undefined)]);
        requireInput(posted && postStatus >= 200 && postStatus < 400,
            'The site did not confirm the submission request.');
        return {submitted: true, http_status: postStatus};
    } finally {
        await context.close();
    }
}

async function readJson(request) {
    let length = 0;
    const chunks = [];
    for await (const chunk of request) {
        length += chunk.length;
        requireInput(length <= LIMIT, 'Browser request is too large.');
        chunks.push(chunk);
    }
    try { return JSON.parse(Buffer.concat(chunks).toString('utf8')); }
    catch { throw new InputError('Invalid browser request.'); }
}

async function serve() {
    const browser = await chromium.launch({
        chromiumSandbox: true,
        proxy: {server: 'http://egress:3128'},
        args: ['--proxy-bypass-list=<-loopback>', '--disable-quic', '--disable-background-networking']
    });
    browser.on('disconnected', () => process.exit(1));
    let active = 0;
    const server = createServer(async (request, response) => {
        const json = (status, data) => {
            response.writeHead(status, {'Content-Type': 'application/json', 'Cache-Control': 'no-store'});
            response.end(JSON.stringify(data));
        };
        if (request.method === 'GET' && request.url === '/health') return json(200, {ok: true});
        if (request.method !== 'POST' || !['/browse', '/inspect', '/submit', '/search'].includes(request.url)) {
            return json(404, {error: 'Unknown browser operation.'});
        }
        if (active >= 4) return json(503, {error: 'Browser worker is busy.'});
        active += 1;
        try {
            json(200, await operate(browser, request.url, await readJson(request)));
        } catch (error) {
            // Playwright errors may include request metadata. Keep them inside the worker.
            json(422, {error: error instanceof InputError ? error.message : 'Browser operation did not finish.'});
        } finally {
            active -= 1;
        }
    });
    server.requestTimeout = 30000;
    await unlink(socketPath).catch(error => { if (error.code !== 'ENOENT') throw error; });
    server.listen(socketPath, () => void chmod(socketPath, 0o660).catch(() => process.exit(1)));
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
    serve().catch(error => {
        // Startup finishes before the worker accepts any user requests.
        console.error('TinyBot browser failed to start with Chromium isolation enabled.', error.message);
        process.exit(1);
    });
}
