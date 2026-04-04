# React Todo Web Application

## Problem Statement

The project needs a working todo web application built with Vite, React, and
Tailwind CSS. The scaffold already has the build system, dependencies, and
Docker configuration in place. The entire application must be implemented in
`src/App.tsx`. All functionality must survive `npm run build` and be served
correctly from nginx via Docker.

## Goals

- Users can add, toggle, delete, and filter todo items in a browser
- Todo state persists across page reloads via localStorage
- The app is styled with Tailwind utility classes
- `npm run build` produces a working dist/ directory
- A Docker image built from the Dockerfile serves the app on port 80

## Requirements

| As a... | I want to... | So that... |
|---------|-------------|-----------|
| user | type a title and click Add | a new todo appears in the list |
| user | click a todo item | it toggles between active and done states |
| user | click the delete button on a todo | the item is permanently removed |
| user | click the All, Active, or Done filter buttons | the list shows only matching todos |
| user | reload the page | my todos are still there (localStorage persistence) |
| developer | run `npm run build` | a dist/ directory is produced without TypeScript errors |

## Scope

- All application code lives in `src/App.tsx`. Do not modify other files.
- Tailwind CSS utility classes for all styling. No custom CSS beyond `index.css`.
- localStorage for persistence. No backend, no external API.

## Constraints

- TypeScript strict mode (already configured in `tsconfig.json`).
- React 19, Vite 6, Tailwind 3 (already in `package.json`).
- The existing `src/main.tsx` imports and renders `App` as the default export.
  Do not modify `src/main.tsx`.
- Use `npm run build` (not `npx vite build`) for all build operations.
- Docker: the Dockerfile is pre-configured with a multi-stage build. Do not
  modify it.

## Contracts

### Data Model

TodoItem TypeScript interface:

```typescript
interface TodoItem {
  id: number;       // Date.now() at creation time
  title: string;    // required, non-empty
  done: boolean;    // false on creation
}
```

### Component API

`App` (default export from `src/App.tsx`):
- State: `todos: TodoItem[]`, `newTitle: string`, `filter: 'all' | 'active' | 'done'`
- `addTodo()`: creates a new TodoItem with `id = Date.now()`, appends to state, saves to localStorage
- `toggleTodo(id: number)`: flips `done` for the matching item, saves to localStorage
- `deleteTodo(id: number)`: removes the matching item from state, saves to localStorage
- `filteredTodos()`: returns todos filtered by the current filter value
- `useEffect` hook: syncs `todos` to `localStorage.setItem('todos', JSON.stringify(todos))` whenever `todos` changes
- On mount: loads from `localStorage.getItem('todos')` with `JSON.parse`, defaults to `[]`

## Acceptance Criteria

| Given... | When... | Then... |
|----------|---------|---------|
| an empty todo list | the user types "Buy milk" and clicks Add | a new todo appears with title "Buy milk" and active state |
| a todo item | the user clicks it | done state toggles; done items show with line-through |
| a todo item | the user clicks its delete button | the item is removed from the list |
| todos in the list | the user clicks Active filter | only incomplete todos are shown |
| todos in the list | the user clicks Done filter | only completed todos are shown |
| todos saved to localStorage | the user reloads the page | all todos are restored |
| the source code exists | `npm run build` runs | exits 0 with no TypeScript errors |
| a built dist/ exists | `docker build` runs | exits 0 producing a valid image |

### Final Validation

```
npm run build
docker build -t loopr-e2e-react-todo .
docker run -d -p 14173:80 loopr-e2e-react-todo
curl -s http://localhost:14173 | grep root
```

## Specs

- **State and logic** - `src/App.tsx` state management: TodoItem interface,
  useState hooks, addTodo, toggleTodo, deleteTodo, filteredTodos, useEffect for
  localStorage sync and initial load. No UI yet - just the wiring.

- **UI components** - `src/App.tsx` render: heading, input + Add button row,
  filter button row (All/Active/Done), todo list. Each todo shows title, toggle
  behavior on click, and a Delete button. Done items get line-through styling.

- **Tailwind styling** - Add Tailwind utility classes throughout: centered
  container (max-w-md mx-auto p-6), styled input and buttons, flex layouts,
  active filter highlighted (bg-blue-500 text-white), done items dimmed
  (line-through text-gray-400), delete button (text-red-500).

## Dependencies

- `node_modules/` already installed by `npm install` in scaffold
- `Dockerfile` and `docker-compose.yml` pre-configured in scaffold
