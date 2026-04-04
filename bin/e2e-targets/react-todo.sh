#!/usr/bin/env bash
# E2E target: Vite + React + Tailwind todo web app in Docker

TARGET_TIMEOUT=1200

scaffold() {
    mkdir -p "${TARGET}"

    # Check Node.js is available
    if ! command -v node &>/dev/null; then
        err "node is not installed"
        exit 1
    fi

    # Check Docker is available
    if ! command -v docker &>/dev/null; then
        err "docker is not installed"
        exit 1
    fi

    # package.json with vite, react, tailwind
    cat > "${TARGET}/package.json" <<'PKG'
{
  "name": "todo-app",
  "private": true,
  "version": "0.1.0",
  "type": "module",
  "scripts": {
    "dev": "vite",
    "build": "vite build",
    "preview": "vite preview",
    "lint": "echo 'no linter configured'",
    "test": "echo 'no tests configured'"
  },
  "dependencies": {
    "react": "^19.1.0",
    "react-dom": "^19.1.0"
  },
  "devDependencies": {
    "@types/react": "^19.1.2",
    "@types/react-dom": "^19.1.2",
    "@vitejs/plugin-react": "^4.4.1",
    "autoprefixer": "^10.4.21",
    "postcss": "^8.5.3",
    "tailwindcss": "^3.4.17",
    "typescript": "^5.8.3",
    "vite": "^6.3.2"
  }
}
PKG

    cat > "${TARGET}/tsconfig.json" <<'TSCONFIG'
{
  "compilerOptions": {
    "target": "ES2020",
    "useDefineForClassFields": true,
    "lib": ["ES2020", "DOM", "DOM.Iterable"],
    "module": "ESNext",
    "skipLibCheck": true,
    "moduleResolution": "bundler",
    "allowImportingTsExtensions": true,
    "isolatedModules": true,
    "moduleDetection": "force",
    "noEmit": true,
    "jsx": "react-jsx",
    "strict": true
  },
  "include": ["src"]
}
TSCONFIG

    cat > "${TARGET}/vite.config.ts" <<'VITE'
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  server: { port: 5173 },
  preview: { port: 4173 },
})
VITE

    cat > "${TARGET}/tailwind.config.js" <<'TAILWIND'
/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{js,ts,jsx,tsx}"],
  theme: { extend: {} },
  plugins: [],
}
TAILWIND

    cat > "${TARGET}/postcss.config.js" <<'POSTCSS'
export default {
  plugins: {
    tailwindcss: {},
    autoprefixer: {},
  },
}
POSTCSS

    cat > "${TARGET}/index.html" <<'HTML'
<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>Todo App</title>
  </head>
  <body>
    <div id="root"></div>
    <script type="module" src="/src/main.tsx"></script>
  </body>
</html>
HTML

    mkdir -p "${TARGET}/src"

    cat > "${TARGET}/src/main.tsx" <<'MAIN'
import React from 'react'
import ReactDOM from 'react-dom/client'
import './index.css'

function App() {
  return <div>Todo App</div>
}

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
)
MAIN

    cat > "${TARGET}/src/index.css" <<'CSS'
@tailwind base;
@tailwind components;
@tailwind utilities;
CSS

    cat > "${TARGET}/src/vite-env.d.ts" <<'DTS'
/// <reference types="vite/client" />
DTS

    cat > "${TARGET}/Dockerfile" <<'DOCKER'
FROM node:20-alpine AS build
WORKDIR /app
COPY package*.json ./
RUN npm ci
COPY . .
RUN npm run build

FROM nginx:alpine
COPY --from=build /app/dist /usr/share/nginx/html
EXPOSE 80
CMD ["nginx", "-g", "daemon off;"]
DOCKER

    cat > "${TARGET}/.dockerignore" <<'DOCKERIGNORE'
node_modules
dist
.git
.gitignore
DOCKERIGNORE

    cat > "${TARGET}/README.md" <<'README'
# Todo App

A todo web application built with Vite, React, and Tailwind CSS.

## Requirements

- Add a todo item with a title (text input + button)
- List all todo items with status indicators
- Mark a todo item as done by clicking it (toggle)
- Delete a todo item with a delete button
- Filter todos: All, Active, Done
- Persist todos to localStorage
- Style with Tailwind CSS utility classes
- Serve from Docker via nginx
README

    # Install dependencies
    log "Installing npm dependencies..."
    (cd "${TARGET}" && npm install --silent 2>&1 | /usr/bin/tail -5)

    (
        cd "${TARGET}"
        git init -q
        echo -e "node_modules/\ndist/\n.env" > .gitignore
        git add -A
        git commit -q -m "init"
    )
    ok "React target ready at ${TARGET}"
}

target_validation_commands() {
    true
}

target_goal() {
    echo "Build a React todo web application using Vite and Tailwind CSS. The app should support: add, list, toggle done, delete, and filter (all/active/done) todos. Persist todos to localStorage. All React code goes in src/App.tsx. Style with Tailwind utility classes. The app must compile with npm run build."
}

target_plan() {
    echo "${LOOPR_ROOT}/bin/e2e-targets/react-todo.md"
}

collect_results() {
    for f in src/App.tsx src/main.tsx Dockerfile; do
        if [[ -f "${TARGET}/${f}" ]]; then
            echo ""
            log "Target ${f}:"
            cat "${TARGET}/${f}"
        fi
    done
}

verify() {
    local pass=true
    local container_id=""

    # Check key files exist
    for f in src/App.tsx src/main.tsx Dockerfile; do
        if [[ -f "${TARGET}/${f}" ]]; then
            ok "${f} exists"
        else
            warn "${f} missing"
            pass=false
        fi
    done

    # npm build
    echo ""
    if (cd "${TARGET}" && npm run build 2>&1 | /usr/bin/tail -10); then
        ok "npm run build succeeded"
    else
        warn "npm run build failed"
        pass=false
    fi

    # Docker build
    echo ""
    if (cd "${TARGET}" && docker build -t loopr-e2e-react-todo . 2>&1 | /usr/bin/tail -10); then
        ok "docker build succeeded"
    else
        warn "docker build failed"
        pass=false
    fi

    # Docker run + curl
    echo ""
    container_id=$(docker run -d -p 14173:80 loopr-e2e-react-todo 2>/dev/null || true)
    if [[ -n "${container_id}" ]]; then
        # Wait for nginx to start
        sleep 2
        if curl -s http://localhost:14173 | grep -q "root"; then
            ok "curl returned HTML with root div"
        else
            warn "curl did not find root div in response"
            pass=false
        fi
        # Cleanup
        docker stop "${container_id}" >/dev/null 2>&1 || true
        docker rm "${container_id}" >/dev/null 2>&1 || true
    else
        warn "docker run failed"
        pass=false
    fi

    if [[ "${pass}" == "true" ]]; then
        ok "All verification checks passed"
    else
        warn "Some verification checks failed"
    fi
}
