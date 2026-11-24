// The page as an installed app: the browser's offer to install it, whether it runs installed, and the
// service worker that keeps it for offline use (dist/sw.js, made by the build: tools/offline.ts).

interface InstallPrompt extends Event {
  prompt(): Promise<void>;
  userChoice: Promise<{ outcome: 'accepted' | 'dismissed' }>;
}

export const pwa = $state({
  /** The browser offers to install the page (beforeinstallprompt), or it runs installed. */
  installable: false,
  installed: false,
  /** The service worker holds the page for offline use; a new version waits for the next start. */
  offline: 'loading' as 'loading' | 'ready' | 'none',
  update: false,
});

let offer: InstallPrompt | null = null;

export function watchInstall(): void {
  const standalone = matchMedia('(display-mode: standalone), (display-mode: fullscreen)');
  pwa.installed = standalone.matches || (navigator as Navigator & { standalone?: boolean }).standalone === true;
  standalone.addEventListener('change', () => (pwa.installed = standalone.matches));
  addEventListener('beforeinstallprompt', (event) => {
    event.preventDefault();
    offer = event as InstallPrompt;
    pwa.installable = true;
  });
  addEventListener('appinstalled', () => {
    offer = null;
    pwa.installable = false;
    pwa.installed = true;
  });
}

/** Shows the browser's install window; false when the browser offers none. */
export async function install(): Promise<boolean> {
  if (!offer) return false;
  await offer.prompt();
  const { outcome } = await offer.userChoice;
  offer = null;
  pwa.installable = false;
  return outcome === 'accepted';
}

/** Registers the service worker. Only a secure page (https, localhost) may have one; the dev server has
 * none, as its modules change on every edit. */
export async function registerWorker(): Promise<void> {
  if (!('serviceWorker' in navigator) || !isSecureContext || import.meta.env.DEV) {
    pwa.offline = 'none';
    return;
  }
  try {
    const base = import.meta.env.BASE_URL;
    const registration = await navigator.serviceWorker.register(`${base}sw.js`, { scope: base });
    pwa.offline = navigator.serviceWorker.controller || registration.active ? 'ready' : 'loading';
    void navigator.serviceWorker.ready.then(() => (pwa.offline = 'ready'));
    // A new version installed while this one runs waits for the next start: a game is never reloaded
    // under the player.
    registration.addEventListener('updatefound', () => {
      const next = registration.installing;
      next?.addEventListener('statechange', () => {
        if (next.state === 'installed' && navigator.serviceWorker.controller) pwa.update = true;
      });
    });
  } catch (error) {
    console.error('the service worker failed:', error);
    pwa.offline = 'none';
  }
}
