import { mount } from 'svelte';
import 'asciinema-player/dist/bundle/asciinema-player.css';
import App from './App.svelte';

const target = document.getElementById('app');
if (!target) throw new Error('missing #app');
export default mount(App, { target });
