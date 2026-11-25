import { execFile } from 'node:child_process';
import { readFile, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { promisify } from 'node:util';
import { InputError, requireInput } from './forms.mjs';

const encode = promisify(execFile);
const outputLimit = 5 * 1024 * 1024;
const rawRecordingLimit = 24 * 1024 * 1024;
const cookieText = /\b(?:cookies?|cookiebeleid|cookietoestemming|cookie[ -]?instellingen|cookieconsent)\b/i;
const cookieHeading = /^(?:cookies?|cookie (?:notice|settings|preferences|consent)|we use cookies|wij gebruiken cookies|cookietoestemming|cookie[ -]?instellingen)[.!?]?$/i;
const genericDecline = /^(?:reject|deny|decline|refuse)(?: all)?[.!]?$|^(?:no thanks|nee bedankt|niet akkoord|alles weigeren|weigeren|afwijzen)[.!]?$/i;
const essential = '(?:functional|essential|necessary|required|strictly necessary)';
const requiredCookieChoice = new RegExp('^(?:' + [
    `(?:(?:allow|accept|use) )?${essential}(?: cookies)? only`,
    `(?:(?:allow|accept|use) )?only ${essential}(?: cookies)?`,
    `(?:allow|accept|use) ${essential}(?: cookies)?`,
    '(?:reject|deny|decline|refuse)(?: all(?: cookies)?| (?:optional|non[- ]essential)(?: cookies)?| cookies)?',
    'no thanks',
    '(?:alleen |uitsluitend )?(?:functionele|noodzakelijke|essentiële) cookies',
    'alleen (?:functioneel|noodzakelijk)',
    '(?:alles |alle cookies )?(?:weigeren|afwijzen)|niet akkoord|nee bedankt',
].join('|') + ')[.!]?$', 'i');
const acceptAllChoice = /^(?:(?:allow all|accept all|agree to all)(?: cookies)?|alles toestaan|alles accepteren|alle cookies accepteren|alle cookies toestaan)[.!]?$/i;
const modal = 'dialog[open],[role="dialog"],[role="alertdialog"],[aria-modal="true"]';
const consentMarker = [
    '[id*="cookie" i]', '[class*="cookie" i]', '[id*="consent" i]', '[class*="consent" i]',
    '[aria-label*="cookie" i]',
].join(',');
const namedRegion = modal + ',' + consentMarker;

export const captureTypes = new Map([['/screenshot', 'image/png'], ['/record', 'video/mp4']]);
export const captureViewport = {width: 960, height: 540};

export function dismissCookieDialog(data) {
    const value = data?.dismiss_cookie_dialog;
    requireInput(value === undefined || typeof value === 'boolean',
        'Expected a boolean dismiss_cookie_dialog option.');
    return value ?? true;
}

function findConsentRegion(element, {selector, words, title, allowAll, generic, plain}) {
    const cookies = new RegExp(words, 'i');
    const heading = new RegExp(title, 'i');
    const affirmative = new RegExp(allowAll, 'i');
    const decline = new RegExp(generic, 'i');
    const label = (element.getAttribute('aria-label') || element.innerText || element.value || '').trim();
    const formAction = element.form && (element.type === 'submit' || element.type === 'reset');
    const navigates = element instanceof HTMLAnchorElement && Boolean(element.href)
        && !element.getAttribute('href')?.startsWith('#');
    if (formAction || navigates || affirmative.test(label) ||
        affirmative.test(element.innerText?.trim() ?? '')) return null;
    if (plain &&
        (!element.matches('div,span') || element.closest('a,button,input,form,label,[role="link"],[role="button"]') ||
         ![...element.parentElement?.children ?? []].some(other => other !== element &&
             other.matches('div,span,button,[role="button"]') &&
             affirmative.test((other.getAttribute('aria-label') || other.textContent || '').trim())))) {
        return null;
    }
    let found = null;
    for (let region = element.parentElement, depth = 0; region && region !== document.body && depth < 6;
        region = region.parentElement ?? region.getRootNode().host, depth++) {
        if (!cookies.test((region.innerText || region.textContent || '').slice(0, 2000))) continue;
        const marked = region.matches(selector);
        const positioned = ['fixed', 'sticky'].includes(getComputedStyle(region).position);
        if (!marked && (plain || !positioned)) continue;
        const headed = heading.test(region.getAttribute('aria-label')?.trim() ?? '') ||
            [...region.querySelectorAll('h1,h2,h3,h4,h5,h6,[role="heading"]')]
                .some(node => heading.test((node.innerText || node.textContent || '').trim()));
        const allowsAll = plain || [...region.querySelectorAll('button,[role="button"],input[type="button"]')]
            .some(other => affirmative.test((other.getAttribute('aria-label') || other.innerText || other.value || '').trim()));
        if (!headed && !allowsAll && (!marked || !label || decline.test(label))) continue;
        found ??= region;
        if (positioned || region.matches('dialog,[role="dialog"],[role="alertdialog"],[aria-modal="true"]')) {
            return region;
        }
    }
    return found;
}

function onscreen(element) {
    const visible = node => {
        const rect = node.getBoundingClientRect();
        return rect.right > 0 && rect.bottom > 0 && rect.left < innerWidth && rect.top < innerHeight &&
            rect.width > 0 && rect.height > 0 &&
            node.checkVisibility({opacityProperty: true, visibilityProperty: true});
    };
    return element.isConnected &&
        (visible(element) || [...element.children].slice(0, 12).some(visible));
}

async function inspectCookieDialogs(page, signal) {
    let visible = false;
    let dormant = false;
    for (const frame of page.frames().slice(0, 12)) {
        signal?.throwIfAborted();
        try {
            if (frame !== page.mainFrame()) {
                const host = await frame.frameElement();
                const bounds = await host.boundingBox();
                await host.dispose();
                if (!bounds || bounds.x >= captureViewport.width || bounds.y >= captureViewport.height ||
                        bounds.x + bounds.width <= 0 || bounds.y + bounds.height <= 0) continue;
            }
            const dialogs = frame.locator(namedRegion).filter({visible: true});
            visible ||= await dialogs.evaluateAll((elements, {modalSelector, words, title, allowAll, required}) => {
                const cookies = new RegExp(words, 'i');
                const heading = new RegExp(title, 'i');
                const affirmative = new RegExp(allowAll, 'i');
                const necessary = new RegExp(required, 'i');
                return elements.some(element => {
                    const rect = element.getBoundingClientRect();
                    if (rect.right <= 0 || rect.bottom <= 0 || rect.left >= innerWidth || rect.top >= innerHeight ||
                            !element.checkVisibility({opacityProperty: true, visibilityProperty: true})) return false;
                    const controls = [...element.querySelectorAll('button,[role="button"],input[type="button"]')]
                        .map(node => (node.getAttribute('aria-label') || node.innerText || node.value || '').trim());
                    const acceptsAll = controls.some(label => affirmative.test(label));
                    if (!element.matches(modalSelector) && !acceptsAll && !controls.some(label => necessary.test(label))) return false;
                    const label = element.getAttribute('aria-label')?.trim() ?? '';
                    const headed = [...element.querySelectorAll('h1,h2,h3,h4,h5,h6,[role="heading"]')]
                        .some(node => heading.test(node.innerText?.trim() ?? ''));
                    return heading.test(label) || headed ||
                        (acceptsAll && cookies.test(element.innerText?.slice(0, 2000) ?? ''));
                });
            }, {
                modalSelector: modal, words: cookieText.source, title: cookieHeading.source,
                allowAll: acceptAllChoice.source, required: requiredCookieChoice.source,
            });
            for (const [choices, plain] of [
                [frame.getByRole('button', {name: requiredCookieChoice, includeHidden: true}), false],
                [frame.getByText(requiredCookieChoice), true],
            ]) {
                const count = Math.min(8, await choices.count());
                for (let index = 0; index < count; index++) {
                    const choice = await choices.nth(index).elementHandle({timeout: 200});
                    if (!choice) continue;
                    let region;
                    let actionable = false;
                    try {
                        region = await choice.evaluateHandle(findConsentRegion, {
                            selector: namedRegion, words: cookieText.source, title: cookieHeading.source,
                            allowAll: acceptAllChoice.source, generic: genericDecline.source, plain,
                        });
                        if (!region.asElement()) continue;
                        const shown = await region.evaluate(onscreen);
                        const choiceShown = await choice.evaluate(onscreen);
                        visible ||= shown || choiceShown;
                        dormant ||= !shown && !choiceShown;
                        actionable = choiceShown;
                        if (actionable) return {visible: true, choice, region};
                    } finally {
                        if (!actionable) {
                            await choice.dispose();
                            await region?.dispose();
                        }
                    }
                }
            }
        } catch {
            // A consent frame can detach or redraw while Chromium is inspecting it.
            signal?.throwIfAborted();
        }
    }
    return {visible, dormant, choice: null};
}

export async function prepareCookieDialog(page, dismiss, signal) {
    const start = performance.now();
    let deadline = start + 400;
    let result;
    do {
        result = await inspectCookieDialogs(page, signal);
        if (result.dormant) deadline = start + 1450;
        if (result.choice || (!dismiss && result.visible) || performance.now() >= deadline) break;
        await delay(100, undefined, {signal});
    } while (true);
    if (!result.choice) return result.visible ? 'visible' : null;
    try {
        if (!dismiss) return 'visible';
        await result.choice.click({timeout: 500});
        const settle = performance.now() + 500;
        do {
            const regionVisible = await result.region.evaluate(onscreen);
            if (!regionVisible) {
                const current = await inspectCookieDialogs(page, signal);
                await current.choice?.dispose();
                await current.region?.dispose();
                if (!current.visible) return 'dismissed';
            }
            if (performance.now() >= settle) return 'visible';
            await delay(100, undefined, {signal});
        } while (true);
    } catch {
        signal?.throwIfAborted();
        return 'visible';
    } finally {
        await result.choice.dispose();
        await result.region.dispose();
    }
}

export function recordingOptions(data) {
    const seconds = data?.duration_seconds ?? 5;
    const scroll = data?.scroll ?? false;
    requireInput(Number.isInteger(seconds) && seconds >= 2 && seconds <= 10,
        'Choose a recording duration from 2 to 10 seconds.');
    requireInput(typeof scroll === 'boolean', 'Expected a boolean scroll option.');
    return {seconds, scroll};
}

export async function screenshot(page) {
    const image = await page.screenshot({type: 'png', timeout: 3500});
    requireInput(image.length > 0 && image.length <= outputLimit, 'Screenshot is too large.');
    return image;
}

export async function recordPage(page, {seconds, scroll}, signal) {
    requireInput(page.video(), 'Browser recording is unavailable.');
    if (scroll) {
        const interval = 250;
        const deadline = performance.now() + seconds * 1000;
        while (performance.now() < deadline - interval) {
            await page.mouse.wheel(0, 100);
            await delay(interval, undefined, {signal});
        }
        await delay(Math.max(0, deadline - performance.now()), undefined, {signal});
    } else {
        await delay(seconds * 1000, undefined, {signal});
    }
    return page.video().path();
}

export async function encodeRecording(path, directory, {seconds}, signal) {
    const original = await stat(path);
    requireInput(original.size > 0 && original.size <= rawRecordingLimit, 'Recording is too large.');
    const target = join(directory, 'capture.mp4');
    try {
        await encode('ffmpeg', [
            '-nostdin', '-v', 'error', '-protocol_whitelist', 'file', '-sseof', `-${seconds}`, '-i', path,
            '-t', String(seconds), '-an', '-c:v', 'libx264', '-threads', '1', '-preset', 'ultrafast',
            '-crf', '25', '-maxrate', '2M', '-bufsize', '2M', '-pix_fmt', 'yuv420p', '-movflags', '+faststart', target,
        ], {timeout: 6000, killSignal: 'SIGKILL', maxBuffer: 8192, signal});
    } catch {
        throw new InputError('The browser recording could not be encoded.');
    }
    const result = await stat(target);
    requireInput(result.size > 0 && result.size <= outputLimit, 'Recording is too large.');
    return readFile(target, {signal});
}
