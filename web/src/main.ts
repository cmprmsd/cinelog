import { mount } from 'svelte';
import 'asciinema-player/dist/bundle/asciinema-player.css';
import './app.css';
import App from './App.svelte';
import { applyTheme, prefs } from './lib/prefs.svelte';

applyTheme(prefs.theme);

const target = document.getElementById('app');
if (!target) throw new Error('missing #app');
export default mount(App, { target });
