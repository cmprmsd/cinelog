// Who is signed in, for views below the shell that need it (annotation authorship).

import type { Me } from './api/types';

export const principal = $state<{ me: Me | null }>({ me: null });
