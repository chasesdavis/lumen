(() => {
  'use strict';

  const root = document.documentElement;
  const mode = document.querySelector('[data-mode]');
  const systemTheme = matchMedia('(prefers-color-scheme: dark)');
  const savedTheme = () => {
    try { return localStorage.getItem('lumen-theme'); } catch { return null; }
  };
  const paintTheme = value => {
    const theme = ['light', 'dark'].includes(value) ? value : systemTheme.matches ? 'dark' : 'light';
    root.dataset.theme = theme;
    const label = theme === 'dark' ? 'Use light theme' : 'Use dark theme';
    mode.setAttribute('aria-label', label);
    mode.title = label;
  };
  let preference = savedTheme();
  paintTheme(preference);
  mode.hidden = false;
  mode.addEventListener('click', () => {
    preference = root.dataset.theme === 'dark' ? 'light' : 'dark';
    try { localStorage.setItem('lumen-theme', preference); } catch {}
    paintTheme(preference);
  });
  systemTheme.addEventListener('change', () => paintTheme(preference));
  window.addEventListener('storage', event => {
    if (event.key === null || event.key === 'lumen-theme') {
      preference = savedTheme();
      paintTheme(preference);
    }
  });

  const story = document.querySelector('.drive-story');
  const pin = story.querySelector('.drive-pin');
  const motion = matchMedia('(prefers-reduced-motion: no-preference) and (min-height: 450px)');
  const links = Array.from(document.querySelectorAll('.island a[href^="#"]'));
  const sections = links.map(link => document.querySelector(link.hash));
  let storyTop = 0, travel = 1, sectionTops = [], scheduled = false, active = -1;
  function render() {
    scheduled = false;
    const y = window.scrollY;
    if (motion.matches) {
      const progress = (y + 56 - storyTop) / travel;
      const frame = progress < .12 ? '0' : progress < .32 ? '1' : progress < .54 ? '2' : '3';
      if (story.dataset.frame !== frame) story.dataset.frame = frame;
    }
    let index = 0;
    sectionTops.forEach((top, i) => { if (y + 96 >= top) index = i; });
    if (index !== active) {
      links.forEach((link, i) => i === index ? link.setAttribute('aria-current', 'location') : link.removeAttribute('aria-current'));
      active = index;
    }
  }
  function measure() {
    story.toggleAttribute('data-interactive', motion.matches);
    if (!motion.matches) delete story.dataset.frame;
    const box = story.getBoundingClientRect();
    storyTop = window.scrollY + box.top;
    travel = Math.max(1, box.height - pin.offsetHeight);
    sectionTops = sections.map(section => section.getBoundingClientRect().top + window.scrollY);
    render();
  }
  measure();
  motion.addEventListener('change', measure);
  if ('ResizeObserver' in window) new ResizeObserver(measure).observe(document.body);
  window.addEventListener('resize', measure);
  window.addEventListener('load', measure, { once: true });
  window.addEventListener('scroll', () => {
    if (!scheduled) { scheduled = true; requestAnimationFrame(render); }
  }, { passive: true });

  if ('IntersectionObserver' in window) {
    const entrance = new IntersectionObserver(entries => {
      for (const entry of entries) {
        if (entry.isIntersecting) { entry.target.classList.add('in'); entrance.unobserve(entry.target); }
      }
    }, { threshold: .08 });
    document.querySelectorAll('.rise').forEach(el => entrance.observe(el));
  }

  const dock = document.querySelector('.tour-dock');
  const dialog = document.getElementById('tour-player');
  const video = dialog.querySelector('video');
  if (typeof dialog.showModal !== 'function') return;
  const teaser = dock.querySelector('.tour-teaser');
  const reduced = matchMedia('(prefers-reduced-motion: reduce)');
  function syncTeaser() {
    if (dock.hidden || dialog.open || document.hidden || reduced.matches || navigator.connection?.saveData) {
      teaser.pause();
      return;
    }
    if (!teaser.hasAttribute('src')) teaser.src = teaser.dataset.preview;
    teaser.play().catch(() => {}); // Keep the poster if autoplay isn't available.
  }
  dock.hidden = false;
  syncTeaser();
  reduced.addEventListener('change', syncTeaser);
  document.addEventListener('visibilitychange', syncTeaser);
  dock.querySelector('.tour-hide').addEventListener('click', () => {
    dock.hidden = true;
    syncTeaser();
    document.querySelector('.island [aria-current]').focus({ preventScroll: true });
  });
  dock.querySelector('.tour-open').addEventListener('click', () => {
    dialog.showModal();
    syncTeaser();
    video.play().catch(() => {}); // Native controls remain available if autoplay is restricted.
  });
  dialog.querySelector('.tour-close').addEventListener('click', () => dialog.close());
  dialog.addEventListener('click', event => {
    if (event.target !== dialog) return;
    const rect = dialog.getBoundingClientRect();
    if (event.clientX < rect.left || event.clientX > rect.right || event.clientY < rect.top || event.clientY > rect.bottom) dialog.close();
  });
  dialog.addEventListener('close', () => { video.pause(); syncTeaser(); });
})();
